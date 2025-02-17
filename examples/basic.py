import bytewax.operators as op
from bytewax.connectors.stdio import StdOutSink
from bytewax.dataflow import Dataflow
from bytewax.testing import TestingSource


def double(x: int) -> int:
    return x * 2


def halve(x: int) -> int:
    return x // 2


def minus_one(x: int) -> int:
    return x - 1


def stringy(x: int) -> str:
    return f"<dance>{x}</dance>"


flow = Dataflow("basic")

inp = op.input("inp", flow, TestingSource(range(10)))
evens, odds = op.branch("e_o", inp, lambda x: x % 2 == 0)
evens = op.map("halve", evens, halve)
odds = op.map("double", odds, double)
combo = op.merge("merge", evens, odds)
combo = op.map("minus_one", combo, minus_one)
combo = op.map("stringy", combo, stringy)
op.output("out", combo, StdOutSink())
