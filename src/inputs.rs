//! Internal code for input.
//!
//! For a user-centric version of input, read the `bytewax.inputs`
//! Python module docstring. Read that first.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::atomic;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use opentelemetry::KeyValue;
use pyo3::create_exception;
use pyo3::exceptions::PyRuntimeError;
use pyo3::exceptions::PyStopIteration;
use pyo3::exceptions::PyTypeError;
use pyo3::intern;
use pyo3::prelude::*;
use timely::dataflow::channels::pact::Pipeline;
use timely::dataflow::operators::generic::builder_rc::OperatorBuilder;
use timely::dataflow::operators::Capability;
use timely::dataflow::ProbeHandle;
use timely::dataflow::Scope;
use timely::dataflow::Stream;
use timely::progress::Timestamp;

use crate::errors::tracked_err;
use crate::errors::PythonException;
use crate::pyo3_extensions::TdPyAny;
use crate::recovery::*;
use crate::timely::*;
use crate::unwrap_any;
use crate::with_timer;

const DEFAULT_COOLDOWN: Duration = Duration::milliseconds(1);

/// Length of epoch.
///
/// Epoch boundaries represent synchronization points between all
/// workers and are when state snapshots are taken and written for
/// recovery.
///
/// The epoch is also used as backpressure within the dataflow; input
/// sources do not start reading new data until all data in the
/// previous epoch has been output and recovery data written.
#[derive(Debug, Copy, Clone, FromPyObject)]
pub(crate) struct EpochInterval(Duration);

impl EpochInterval {
    /// Determine how many epochs (rounded up) exist in some other
    /// duration.
    ///
    /// Useful for calculating GC commit epochs.
    pub(crate) fn epochs_per(&self, other: Duration) -> u64 {
        (other.num_milliseconds() as f64 / self.0.num_milliseconds() as f64)
            // Round up so we always have at least the backup interval
            // time. Unless it's 0, then it's ok. The integer part of
            // the result will always fit into a u64 so chopping off
            // bits should be fine.
            .ceil() as u64
    }
}

#[test]
fn test_epochs_per() {
    let found =
        EpochInterval(Duration::milliseconds(5000)).epochs_per(Duration::milliseconds(12000));
    assert_eq!(found, 3);
}

#[test]
fn test_epochs_per_zero() {
    let found = EpochInterval(Duration::milliseconds(5000)).epochs_per(Duration::milliseconds(0));
    assert_eq!(found, 0);
}

impl Default for EpochInterval {
    fn default() -> Self {
        Self(Duration::seconds(10))
    }
}

create_exception!(
    bytewax.inputs,
    AbortExecution,
    PyRuntimeError,
    "Raise this from `next_batch` to abort for testing purposes."
);

/// Represents a `bytewax.inputs.Source` from Python.
#[derive(Clone)]
pub(crate) struct Source(Py<PyAny>);

/// Do some eager type checking.
impl<'source> FromPyObject<'source> for Source {
    fn extract(ob: &'source PyAny) -> PyResult<Self> {
        let abc = ob
            .py()
            .import("bytewax.inputs")?
            .getattr("Source")?
            .extract()?;
        if !ob.is_instance(abc)? {
            Err(PyTypeError::new_err(
                "source must subclass `bytewax.inputs.Source`",
            ))
        } else {
            Ok(Self(ob.into()))
        }
    }
}

impl IntoPy<Py<PyAny>> for Source {
    fn into_py(self, _py: Python<'_>) -> Py<PyAny> {
        self.0
    }
}

impl ToPyObject for Source {
    fn to_object(&self, py: Python<'_>) -> PyObject {
        self.0.to_object(py)
    }
}

impl Source {
    pub(crate) fn extract<'p, D>(&'p self, py: Python<'p>) -> PyResult<D>
    where
        D: FromPyObject<'p>,
    {
        self.0.extract(py)
    }
}

/// Represents a `bytewax.inputs.FixedPartitionedSource` from Python.
#[derive(Clone)]
pub(crate) struct FixedPartitionedSource(Py<PyAny>);

/// Do some eager type checking.
impl<'source> FromPyObject<'source> for FixedPartitionedSource {
    fn extract(ob: &'source PyAny) -> PyResult<Self> {
        let abc = ob
            .py()
            .import("bytewax.inputs")?
            .getattr("FixedPartitionedSource")?
            .extract()?;
        if !ob.is_instance(abc)? {
            Err(PyTypeError::new_err(
                "fixed partitioned source must subclass `bytewax.inputs.FixedPartitionedSource`",
            ))
        } else {
            Ok(Self(ob.into()))
        }
    }
}

fn default_next_awake(
    res: Option<DateTime<Utc>>,
    batch_len: usize,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if let Some(next_awake) = res {
        // If the source returned an explicit next awake time, always
        // oblige.
        Some(next_awake)
    } else {
        // If `next_awake` returned `None`, then do the default
        // behavior:
        if batch_len > 0 {
            // Re-awaken immediately if there were items in the batch.
            None
        } else {
            // Wait a cooldown before re-awakening if there were no
            // items in the batch.
            Some(now + DEFAULT_COOLDOWN)
        }
    }
}

struct PartitionedPartState {
    part: StatefulPartition,
    downstream_cap: Capability<u64>,
    snap_cap: Capability<u64>,
    epoch_started: DateTime<Utc>,
    next_awake: Option<DateTime<Utc>>,
}

impl PartitionedPartState {
    fn awake_due(&self, now: DateTime<Utc>) -> bool {
        match self.next_awake {
            None => true,
            Some(next_awake) => now >= next_awake,
        }
    }
}

impl FixedPartitionedSource {
    fn list_parts(&self, py: Python) -> PyResult<Vec<StateKey>> {
        self.0.call_method0(py, "list_parts")?.extract(py)
    }

    fn build_part(
        &self,
        py: Python,
        now: DateTime<Utc>,
        for_part: &StateKey,
        resume_state: Option<TdPyAny>,
    ) -> PyResult<StatefulPartition> {
        self.0
            .call_method1(py, "build_part", (now, for_part.clone(), resume_state))?
            .extract(py)
    }

    /// Read items from a partitioned input.
    ///
    /// Will manage assigning primary workers for all partitions and
    /// building them.
    ///
    /// This can't be unified into the recovery system input operators
    /// because they are stateless and have no epoch semantics.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn partitioned_input<S>(
        self,
        py: Python,
        scope: &mut S,
        step_id: StepId,
        epoch_interval: EpochInterval,
        probe: &ProbeHandle<u64>,
        abort: &Arc<AtomicBool>,
        start_at: ResumeEpoch,
        loads: &Stream<S, Snapshot>,
    ) -> PyResult<(Stream<S, TdPyAny>, Stream<S, Snapshot>)>
    where
        S: Scope<Timestamp = u64>,
    {
        let this_worker = scope.w_index();

        let local_parts = self.list_parts(py).reraise_with(|| {
            format!("error calling `FixedPartitionSource.list_parts` in step {step_id}")
        })?;
        let all_parts = local_parts.into_broadcast(scope, S::Timestamp::minimum());
        let primary_updates = all_parts.assign_primaries(format!("{step_id}.assign_primaries"));

        let routed_loads = loads
            .filter_snaps(step_id.clone())
            .route(format!("{step_id}.loads_route"), &primary_updates);

        let op_name = format!("{step_id}.partitioned_input");
        let mut op_builder = OperatorBuilder::new(op_name.clone(), scope.clone());

        let mut loads_input = op_builder.new_input(&routed_loads, routed_exchange());
        let mut primaries_input = op_builder.new_input(&primary_updates, Pipeline);

        let (mut downstream_output, downstream) = op_builder.new_output();
        let (mut snaps_output, snaps) = op_builder.new_output();

        let info = op_builder.operator_info();
        let activator = scope.activator_for(&info.address[..]);
        let probe = probe.clone();
        let abort = abort.clone();

        let meter = opentelemetry::global::meter("bytewax");
        let item_out_count = meter
            .u64_counter("item_out_count")
            .with_description("number of items this step has emitted")
            .init();
        let next_batch_histogram = meter
            .f64_histogram("inp_part_next_batch_duration_seconds")
            .with_description("`next_batch` duration in seconds")
            .init();
        let snapshot_histogram = meter
            .f64_histogram("snapshot_duration_seconds")
            .with_description("`snapshot` duration in seconds")
            .init();
        let labels = vec![
            KeyValue::new("step_id", step_id.0.to_string()),
            KeyValue::new("worker_index", this_worker.0.to_string()),
        ];

        op_builder.build(move |mut init_caps| {
            init_caps.downgrade_all(&start_at.0);
            let init_snap_cap = init_caps.pop().unwrap();
            let init_downstream_cap = init_caps.pop().unwrap();
            let mut init_caps = Some((init_downstream_cap, init_snap_cap));

            let mut parts: BTreeMap<StateKey, PartitionedPartState> = BTreeMap::new();
            let mut primary_parts = BTreeSet::new();

            let mut tmp = Vec::new();
            let mut primaries_inbuf = InBuffer::new();

            move |input_frontiers| {
                tracing::debug_span!("operator", operator = op_name).in_scope(|| {
                    let now = Utc::now();

                    primaries_input.for_each(|cap, incoming| {
                        let epoch = cap.time();
                        primaries_inbuf.extend(*epoch, incoming);
                    });

                    loads_input.for_each(|cap, incoming| {
                        let load_epoch = cap.time();
                        assert!(tmp.is_empty());
                        incoming.swap(&mut tmp);

                        // Snapshots might be from an "old" epoch if
                        // there were no items and thus snapshots
                        // stored during a more recent epoch, so
                        // ensure that we always FFWd the capabilities
                        // to where this execution should start.
                        let emit_epoch = std::cmp::max(*load_epoch, start_at.0);

                        unwrap_any!(Python::with_gil(|py| -> PyResult<()> {
                            for (worker, (part_key, change)) in tmp.drain(..) {
                                assert!(worker == this_worker);

                                if let StateChange::Upsert(state) = change {
                                    tracing::info!("Resuming {part_key:?} at epoch {emit_epoch} with state {state:?}");
                                    let part = self.build_part(
                                        py,
                                        now,
                                        &part_key,
                                        Some(state)
                                    ).reraise_with(|| format!("error calling `FixedPartitionSource.build_part` in step {step_id} for partition {part_key}"))?;
                                    let next_awake = part.next_awake(py)
                                        .reraise_with(|| format!("error calling `StatefulSourcePartition.next_awake` in step {step_id} for partition {part_key}"))?;

                                    let state = PartitionedPartState {
                                        part,
                                        downstream_cap: cap.delayed_for_output(&emit_epoch, 0),
                                        snap_cap: cap.delayed_for_output(&emit_epoch, 1),
                                        epoch_started: now,
                                        next_awake,
                                    };
                                    parts.insert(part_key, state);
                                }
                            }

                            Ok(())
                        }));
                    });

                    // Apply this worker's primary assignments in
                    // epoch order. We don't need a notificator here
                    // because we don't need any capability
                    // management.
                    let primaries_frontier = &input_frontiers[1];
                    let closed_primaries_epochs: Vec<_> = primaries_inbuf
                        .epochs()
                        .filter(|e| primaries_frontier.is_closed(e))
                        .collect();
                    for epoch in closed_primaries_epochs {
                        if let Some(primaries) = primaries_inbuf.remove(&epoch) {
                            for (part, worker) in primaries {
                                if worker == this_worker {
                                    primary_parts.insert(part);
                                }
                            }
                        }
                    }

                    // Init any partitions that didn't have load data
                    // once the loads input is EOF.
                    let loads_frontier = &input_frontiers[0];
                    if loads_frontier.is_eof() {
                        // We take this out of the Option so we drop the
                        // init caps and they don't linger.
                        if let Some((init_downstream_cap, init_snap_cap)) = init_caps.take() {
                            assert!(*init_downstream_cap.time() == *init_snap_cap.time());
                            let epoch = init_downstream_cap.time();

                            // This is a slight abuse of epoch
                            // semantics since have no way of
                            // synchronizing the evolution of
                            // `primary_parts` with the EOF of the
                            // load stream. But it's fine since we're
                            // never going to open up the loads stream
                            // again.
                            unwrap_any!(Python::with_gil(|py| -> PyResult<()> {
                                for part_key in &primary_parts {
                                    if !parts.contains_key(part_key) {
                                        tracing::info!("Init-ing {part_key:?} at epoch {epoch:?}");
                                        let part = self.build_part(py, now, part_key, None)
                                            .reraise_with(|| format!("error calling `FixedPartitionSource.build_part` in step {step_id} for partition {part_key}"))?;
                                        let next_awake = part.next_awake(py)
                                            .reraise_with(|| format!("error calling `StatefulSourcePartition.next_awake` in step {step_id} for partition {part_key}"))?;

                                        let part_state = PartitionedPartState {
                                            part,
                                            downstream_cap:  init_downstream_cap.clone(),
                                            snap_cap: init_snap_cap.clone(),
                                            epoch_started: now,
                                            next_awake,
                                        };
                                        parts.insert(part_key.clone(), part_state);
                                    }
                                }

                                Ok(())
                            }));
                        }
                    }

                    let mut eofd_parts_buffer = Vec::new();

                    let mut downstream_handle = downstream_output.activate();
                    let mut snaps_handle = snaps_output.activate();
                    for (part_key, part_state) in parts.iter_mut() {
                        tracing::trace_span!("partition", part_key = ?part_key).in_scope(|| {
                            assert!(
                                *part_state.downstream_cap.time() == *part_state.snap_cap.time()
                            );
                            let epoch = *part_state.downstream_cap.time();

                            // When we increment the epoch for this
                            // partition, wait until all ouputs have
                            // finished the previous epoch before
                            // emitting more data to have
                            // backpressure.
                            if !probe.less_than(&epoch) && part_state.awake_due(now) {
                                unwrap_any!(Python::with_gil(|py| -> PyResult<()> {
                                    let batch_res = with_timer!(
                                        next_batch_histogram,
                                        labels,
                                        part_state
                                            .part
                                            .next_batch(py, part_state.next_awake)
                                            .reraise_with(|| format!("error calling `StatefulSourcePartition.next_batch` in step {step_id} for partition {part_key}"))?
                                    );

                                    let mut eof = false;
                                    match batch_res {
                                        BatchResult::Batch(batch) => {
                                            let batch_len = batch.len();

                                            let mut downstream_session = downstream_handle.session(&part_state.downstream_cap);
                                            item_out_count.add(batch_len as u64, &labels);
                                            let mut batch = batch.into_iter().map(TdPyAny::from).collect();
                                            downstream_session.give_vec(&mut batch);

                                            let next_awake_res = part_state
                                                .part
                                                .next_awake(py)
                                                .reraise_with(|| format!("error calling `StatefulSourcePartition.next_awake` in step {step_id} for partition {part_key}"))?;

                                            part_state.next_awake = default_next_awake(next_awake_res, batch_len, now);
                                        },
                                        BatchResult::Eof => {
                                            eof = true;
                                            eofd_parts_buffer.push(part_key.clone());
                                            tracing::debug!("EOFd");
                                        },
                                        BatchResult::Abort => {
                                            abort.store(true, atomic::Ordering::Relaxed);
                                            tracing::debug!("EOFd");
                                        }
                                    }

                                    // Increment the epoch for this
                                    // partition when the interval
                                    // elapses or the input is EOF. We
                                    // increment and snapshot on EOF
                                    // so that we capture the offsets
                                    // for this partition to resume
                                    // from; we produce the same
                                    // behavior as if this partition
                                    // would have lived to the next
                                    // epoch interval. Only increment
                                    // once we've caught up (in this
                                    // if-block) otherwise you can get
                                    // cascading advancement and never
                                    // poll input.
                                    if now - part_state.epoch_started >= epoch_interval.0 || eof {
                                        let state = with_timer!(
                                            snapshot_histogram,
                                            labels,
                                            part_state.part
                                                .snapshot(py)
                                                .reraise_with(|| format!("error calling `StatefulSourcePartition.snapshot` in step {step_id} for partition {part_key}"))?
                                        );
                                        tracing::trace!("End of epoch {epoch} partition state now {state:?}");
                                        let snap = Snapshot(
                                            step_id.clone(),
                                            part_key.clone(),
                                            StateChange::Upsert(state),
                                        );

                                        let mut snaps_session = snaps_handle.session(&part_state.snap_cap);
                                        snaps_session.give(snap);

                                        let next_epoch = epoch + 1;
                                        part_state.downstream_cap.downgrade(&next_epoch);
                                        part_state.snap_cap.downgrade(&next_epoch);
                                        part_state.epoch_started = now;
                                        tracing::debug!("Advanced to epoch {next_epoch}");
                                    }

                                    Ok(())
                                }));
                            }
                        });
                    }

                    for part in eofd_parts_buffer {
                        parts.remove(&part);
                    }

                    if !loads_frontier.is_eof() {
                        // If we're not done loading, don't explicitly
                        // request activation so we will only be
                        // awoken when there's new loading input and
                        // we don't spin during loading.
                    } else if !parts.is_empty() {
                        if let Some(min_next_awake) = parts.values().map(|part_state| part_state.next_awake.unwrap_or(now)).min() {
                            let awake_after = min_next_awake - now;
                            // If we are already late for the next
                            // activation, awake immediately.
                            let awake_after = awake_after.to_std().unwrap_or(std::time::Duration::ZERO);
                            activator.activate_after(awake_after);
                        }
                    }
                });
            }
        });

        Ok((downstream, snaps))
    }
}

/// Represents a `bytewax.inputs.StatefulSourcePartition` in Python.
struct StatefulPartition(Py<PyAny>);

/// Do some eager type checking.
impl<'source> FromPyObject<'source> for StatefulPartition {
    fn extract(ob: &'source PyAny) -> PyResult<Self> {
        let abc = ob
            .py()
            .import("bytewax.inputs")?
            .getattr("StatefulSourcePartition")?
            .extract()?;
        if !ob.is_instance(abc)? {
            Err(tracked_err::<PyTypeError>(
                "stateful source partition must subclass `bytewax.inputs.StatefulSourcePartition`",
            ))
        } else {
            Ok(Self(ob.into()))
        }
    }
}

enum BatchResult {
    Eof,
    Abort,
    Batch(Vec<PyObject>),
}

impl StatefulPartition {
    fn next_batch(&self, py: Python, sched: Option<DateTime<Utc>>) -> PyResult<BatchResult> {
        match self
            .0
            .as_ref(py)
            .call_method1(intern!(py, "next_batch"), (sched,))
        {
            Err(err) if err.is_instance_of::<PyStopIteration>(py) => Ok(BatchResult::Eof),
            Err(err) if err.is_instance_of::<AbortExecution>(py) => Ok(BatchResult::Abort),
            Err(err) => Err(err),
            Ok(obj) => {
                let iter = obj.iter().reraise_with(|| {
                    format!(
                        "`next_batch` must return an iterable; got a `{}` instead",
                        unwrap_any!(obj.get_type().name()),
                    )
                })?;
                let batch = iter
                    .map(|res| res.map(PyObject::from))
                    .collect::<PyResult<Vec<_>>>()
                    .reraise("error while iterating through batch")?;
                Ok(BatchResult::Batch(batch))
            }
        }
    }

    fn next_awake(&self, py: Python) -> PyResult<Option<DateTime<Utc>>> {
        self.0
            .call_method0(py, intern!(py, "next_awake"))?
            .extract(py)
    }

    fn snapshot(&self, py: Python) -> PyResult<TdPyAny> {
        Ok(self.0.call_method0(py, intern!(py, "snapshot"))?.into())
    }

    fn close(&self, py: Python) -> PyResult<()> {
        let _ = self.0.call_method0(py, "close");
        Ok(())
    }
}

impl Drop for StatefulPartition {
    fn drop(&mut self) {
        unwrap_any!(Python::with_gil(|py| self
            .close(py)
            .reraise("error closing StatefulSourcePartition")));
    }
}

/// Represents a `bytewax.inputs.DynamicInput` from Python.
#[derive(Clone)]
pub(crate) struct DynamicSource(Py<PyAny>);

/// Do some eager type checking.
impl<'source> FromPyObject<'source> for DynamicSource {
    fn extract(ob: &'source PyAny) -> PyResult<Self> {
        let abc = ob
            .py()
            .import("bytewax.inputs")?
            .getattr("DynamicSource")?
            .extract()?;
        if !ob.is_instance(abc)? {
            Err(tracked_err::<PyTypeError>(
                "dynamic source must subclass `bytewax.inputs.DynamicSource`",
            ))
        } else {
            Ok(Self(ob.into()))
        }
    }
}

struct DynamicPartState {
    part: StatelessPartition,
    output_cap: Capability<u64>,
    epoch_started: DateTime<Utc>,
    next_awake: Option<DateTime<Utc>>,
}

impl DynamicPartState {
    fn awake_due(&self, now: DateTime<Utc>) -> bool {
        match self.next_awake {
            None => true,
            Some(next_awake) => now >= next_awake,
        }
    }
}

impl DynamicSource {
    fn build(
        &self,
        py: Python,
        now: DateTime<Utc>,
        index: WorkerIndex,
        count: WorkerCount,
    ) -> PyResult<StatelessPartition> {
        self.0
            .call_method1(py, "build", (now, index.0, count.0))?
            .extract(py)
    }

    /// Read items from a dynamic input.
    ///
    /// Will manage automatically building sources. All you have to do
    /// is pass in the definition.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dynamic_input<S>(
        self,
        py: Python,
        scope: &S,
        step_id: StepId,
        epoch_interval: EpochInterval,
        probe: &ProbeHandle<u64>,
        abort: &Arc<AtomicBool>,
        start_at: ResumeEpoch,
    ) -> PyResult<Stream<S, TdPyAny>>
    where
        S: Scope<Timestamp = u64>,
    {
        let now = Utc::now();
        let worker_index = scope.w_index();
        let worker_count = scope.w_count();
        let part = self
            .build(py, now, worker_index, worker_count)
            .reraise_with(|| format!("error calling `DynamicSource.build` in step {step_id}"))?;

        let op_name = format!("{step_id}.dynamic_input");
        let mut op_builder = OperatorBuilder::new(op_name.clone(), scope.clone());

        let (mut downstream_wrapper, downstream) = op_builder.new_output();

        let info = op_builder.operator_info();
        let activator = scope.activator_for(&info.address[..]);
        let probe = probe.clone();
        let abort = abort.clone();

        let meter = opentelemetry::global::meter("bytewax");
        let item_out_count = meter
            .u64_counter("item_out_count")
            .with_description("number of items this step has emitted")
            .init();
        let next_batch_histogram = meter
            .f64_histogram("inp_part_next_batch_duration_seconds")
            .with_description("`next_batch` duration in seconds")
            .init();
        let labels = vec![
            KeyValue::new("step_id", step_id.0.to_string()),
            KeyValue::new("worker_index", worker_index.0.to_string()),
        ];

        op_builder.build(move |mut init_caps| {
            let now = Utc::now();

            // Inputs must init to the resume epoch.
            init_caps.downgrade_all(&start_at.0);
            let output_cap = init_caps.pop().unwrap();

            let next_awake = unwrap_any!(Python::with_gil(|py| part
                .next_awake(py)
                .reraise("error getting next awake time")));
            let mut part_state = Some(DynamicPartState {
                part,
                output_cap,
                epoch_started: now,
                next_awake,
            });

            move |_input_frontiers| {
                tracing::debug_span!("operator", operator = op_name).in_scope(|| {
                    let now = Utc::now();

                    let mut eof = false;

                    let mut downstream_handle = downstream_wrapper.activate();
                    if let Some(part_state) = &mut part_state {
                        let epoch = part_state.output_cap.time();

                        // When we increment the epoch for this
                        // partition, wait until all ouputs have
                        // finished the previous epoch before emitting
                        // more data to have backpressure.
                        if !probe.less_than(epoch) && part_state.awake_due(now) {
                            unwrap_any!(Python::with_gil(|py| -> PyResult<()> {
                                let res = with_timer!(
                                    next_batch_histogram,
                                    labels,
                                    part_state
                                        .part
                                        .next_batch(py, part_state.next_awake)
                                        .reraise_with(|| format!("error calling `StatelessSourcePartition.next_batch` in step {step_id}"))?
                                );

                                match res {
                                    BatchResult::Batch(batch) => {
                                        let batch_len = batch.len();

                                        let mut downstream_session = downstream_handle.session(&part_state.output_cap);
                                        item_out_count.add(batch_len as u64, &labels);
                                        let mut batch = batch.into_iter().map(TdPyAny::from).collect();
                                        downstream_session.give_vec(&mut batch);

                                        let next_awake_res = part_state
                                            .part
                                            .next_awake(py)
                                            .reraise_with(|| format!("error calling `StatelessSourcePartition.next_awake` in step {step_id}"))?;

                                        part_state.next_awake =
                                            default_next_awake(next_awake_res, batch_len, now);
                                    }
                                    BatchResult::Eof => {
                                        eof = true;
                                        tracing::trace!("EOFd");
                                    }
                                    BatchResult::Abort => {
                                        abort.store(true, atomic::Ordering::Relaxed);
                                        tracing::error!("Aborted");
                                    }
                                }

                                Ok(())
                            }));

                            // Only increment once we've caught up (in
                            // this if-block) otherwise you can get
                            // cascading advancement and never poll
                            // input.
                            if now - part_state.epoch_started >= epoch_interval.0 {
                                let next_epoch = epoch + 1;
                                part_state.epoch_started = now;
                                part_state.output_cap.downgrade(&next_epoch);
                                tracing::trace!("Advanced to epoch {next_epoch}");
                            }
                        }
                    }

                    if eof {
                        part_state = None;
                    }

                    if let Some(part_state) = &part_state {
                        if let Some(next_awake) = part_state.next_awake {
                            let awake_after = next_awake - now;
                            // If we are already late for the next
                            // activation, awake immediately.
                            let awake_after = awake_after.to_std().unwrap_or(std::time::Duration::ZERO);
                            activator.activate_after(awake_after);
                        } else {
                            activator.activate();
                        }
                    }
                });
            }
        });

        Ok(downstream)
    }
}

/// Represents a `bytewax.inputs.StatelessSource` in Python.
struct StatelessPartition(Py<PyAny>);

/// Do some eager type checking.
impl<'source> FromPyObject<'source> for StatelessPartition {
    fn extract(ob: &'source PyAny) -> PyResult<Self> {
        let abc = ob
            .py()
            .import("bytewax.inputs")?
            .getattr("StatelessSourcePartition")?
            .extract()?;
        if !ob.is_instance(abc)? {
            Err(tracked_err::<PyTypeError>(
                "stateless source partition must subclass `bytewax.inputs.StatelessSourcePartition`",
            ))
        } else {
            Ok(Self(ob.into()))
        }
    }
}

impl StatelessPartition {
    fn next_batch(&self, py: Python, sched: Option<DateTime<Utc>>) -> PyResult<BatchResult> {
        match self
            .0
            .as_ref(py)
            .call_method1(intern!(py, "next_batch"), (sched,))
        {
            Err(err) if err.is_instance_of::<PyStopIteration>(py) => Ok(BatchResult::Eof),
            Err(err) if err.is_instance_of::<AbortExecution>(py) => Ok(BatchResult::Abort),
            Err(err) => Err(err),
            Ok(obj) => {
                let iter = obj.iter().reraise_with(|| {
                    format!(
                        "`next_batch` must return an iterable; got a `{}` instead",
                        unwrap_any!(obj.get_type().name()),
                    )
                })?;
                let batch = iter
                    .map(|res| res.map(PyObject::from))
                    .collect::<PyResult<Vec<_>>>()
                    .reraise("error while iterating through batch")?;
                Ok(BatchResult::Batch(batch))
            }
        }
    }

    fn next_awake(&self, py: Python) -> PyResult<Option<DateTime<Utc>>> {
        self.0
            .call_method0(py, intern!(py, "next_awake"))?
            .extract(py)
    }

    fn close(&self, py: Python) -> PyResult<()> {
        let _ = self.0.call_method0(py, "close")?;
        Ok(())
    }
}

impl Drop for StatelessPartition {
    fn drop(&mut self) {
        unwrap_any!(Python::with_gil(|py| self
            .close(py)
            .reraise("error closing StatelessSourcePartition")));
    }
}

pub(crate) fn register(py: Python, m: &PyModule) -> PyResult<()> {
    m.add("AbortExecution", py.get_type::<AbortExecution>())?;
    Ok(())
}
