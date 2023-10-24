import json
import operator
from datetime import datetime, timedelta, timezone

# pip install aiohttp-sse-client
from aiohttp_sse_client.client import EventSource
from bytewax.connectors.stdio import StdOutSink
from bytewax.dataflow import Dataflow
from bytewax.inputs import FixedPartitionedSource, StatefulSourcePartition, batch_async
from bytewax.window import SystemClockConfig, TumblingWindow


async def _sse_agen(url):
    async with EventSource(url) as source:
        async for event in source:
            yield event.data


class WikiPartition(StatefulSourcePartition):
    def __init__(self):
        agen = _sse_agen("https://stream.wikimedia.org/v2/stream/recentchange")
        # Gather up to 0.25 sec of or 1000 items.
        self._batcher = batch_async(agen, timedelta(seconds=0.25), 1000)

    def next_batch(self):
        return next(self._batcher)

    def snapshot(self):
        return None


class WikiSource(FixedPartitionedSource):
    def list_parts(self):
        return ["single-part"]

    def build_part(self, for_key, resume_state):
        assert for_key == "single-part"
        assert resume_state is None
        return WikiPartition()


def initial_count(data_dict):
    return data_dict["server_name"], 1


def keep_max(max_count, metadata__new_count):
    _metadata, new_count = metadata__new_count
    new_max = max(max_count, new_count)
    # print(f"Just got {new_count}, old max was {max_count}, new max is {new_max}")
    return new_max, new_max


flow = Dataflow()
flow.input("inp", WikiSource())
# "event_json"
flow.map("load_json", json.loads)
# {"server_name": "server.name", ...}
flow.map("initial_count", initial_count)
# ("server.name", 1)
flow.reduce_window(
    "sum",
    SystemClockConfig(),
    TumblingWindow(
        length=timedelta(seconds=2), align_to=datetime(2023, 1, 1, tzinfo=timezone.utc)
    ),
    operator.add,
)
# ("server.name", (metadata, sum_per_window))
flow.stateful_map("keep_max", lambda: 0, keep_max)
# ("server.name", max_per_window)
flow.map("format", lambda x: (x[0], f"{x[0]}, {x[1]}"))
flow.output("out", StdOutSink())
