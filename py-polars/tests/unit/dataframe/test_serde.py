from __future__ import annotations

import io
import pickle
from datetime import date, datetime, timedelta
from decimal import Decimal as D
from multiprocessing.pool import ThreadPool
from typing import TYPE_CHECKING, Any

import pytest
from hypothesis import example, given

import polars as pl
from polars.exceptions import ComputeError
from polars.testing import assert_frame_equal
from polars.testing.parametric import dataframes

if TYPE_CHECKING:
    from pathlib import Path

    from polars._typing import SerializationFormat


def test_df_serde_roundtrip_binary(df: pl.DataFrame) -> None:
    serialized = df.serialize()
    result = pl.DataFrame.deserialize(io.BytesIO(serialized), format="binary")
    assert_frame_equal(result, df, categorical_as_str=True)


@given(df=dataframes())
@example(df=pl.DataFrame({"a": [None, None]}, schema={"a": pl.Null}))
@example(df=pl.DataFrame(schema={"a": pl.List(pl.String)}))
def test_df_serde_roundtrip_json(df: pl.DataFrame) -> None:
    serialized = df.serialize(format="json")
    result = pl.DataFrame.deserialize(io.StringIO(serialized), format="json")

    if isinstance(dt := df.to_series(0).dtype, pl.Decimal):
        if dt.precision is None:
            # This gets converted to precision 38 upon `to_arrow()`
            pytest.skip("precision None")

    assert_frame_equal(result, df, categorical_as_str=True)


def test_df_serde(df: pl.DataFrame) -> None:
    serialized = df.serialize()
    assert isinstance(serialized, bytes)
    result = pl.DataFrame.deserialize(io.BytesIO(serialized))
    assert_frame_equal(result, df)


def test_df_serde_json_stringio(df: pl.DataFrame) -> None:
    serialized = df.serialize(format="json")
    assert isinstance(serialized, str)
    result = pl.DataFrame.deserialize(io.StringIO(serialized), format="json")
    assert_frame_equal(result, df)


def test_df_serialize_json() -> None:
    df = pl.DataFrame({"a": [1, 2, 3], "b": [9, 5, 6]}).sort("a")
    result = df.serialize(format="json")

    assert isinstance(result, str)

    f = io.StringIO(result)

    assert_frame_equal(pl.DataFrame.deserialize(f, format="json"), df)


@pytest.mark.parametrize(
    ("format", "buf"),
    [
        ("binary", io.BytesIO()),
        ("json", io.StringIO()),
        ("json", io.BytesIO()),
    ],
)
def test_df_serde_to_from_buffer(
    df: pl.DataFrame, format: SerializationFormat, buf: io.IOBase
) -> None:
    df.serialize(buf, format=format)
    buf.seek(0)
    read_df = pl.DataFrame.deserialize(buf, format=format)
    assert_frame_equal(df, read_df, categorical_as_str=True)


@pytest.mark.write_disk
def test_df_serde_to_from_file(df: pl.DataFrame, tmp_path: Path) -> None:
    tmp_path.mkdir(exist_ok=True)

    file_path = tmp_path / "small.bin"
    df.serialize(file_path)
    out = pl.DataFrame.deserialize(file_path)

    assert_frame_equal(df, out, categorical_as_str=True)


def test_df_serde2(df: pl.DataFrame) -> None:
    # Text-based conversion loses time info
    df = df.select(pl.all().exclude(["cat", "time"]))
    s = df.serialize()
    f = io.BytesIO()
    f.write(s)
    f.seek(0)
    out = pl.DataFrame.deserialize(f)
    assert_frame_equal(out, df)

    file = io.BytesIO()
    df.serialize(file)
    file.seek(0)
    out = pl.DataFrame.deserialize(file)
    assert_frame_equal(out, df)


def test_df_serde_enum() -> None:
    dtype = pl.Enum(["foo", "bar", "ham"])
    df = pl.DataFrame([pl.Series("e", ["foo", "bar", "ham"], dtype=dtype)])
    buf = io.BytesIO()
    df.serialize(buf)
    buf.seek(0)
    df_in = pl.DataFrame.deserialize(buf)
    assert df_in.schema["e"] == dtype


@pytest.mark.parametrize(
    ("data", "dtype"),
    [
        ([[1, 2, 3], [None, None, None], [1, None, 3]], pl.Array(pl.Int32(), shape=3)),
        ([["a", "b"], [None, None]], pl.Array(pl.Utf8, shape=2)),
        ([[True, False, None], [None, None, None]], pl.Array(pl.Boolean, shape=3)),
        (
            [[[1, 2, 3], [4, None, 5]], None, [[None, None, 2]]],
            pl.List(pl.Array(pl.Int32(), shape=3)),
        ),
        (
            [
                [datetime(1991, 1, 1), datetime(1991, 1, 1), None],
                [None, None, None],
            ],
            pl.Array(pl.Datetime, shape=3),
        ),
        (
            [[D("1.0"), D("2.0"), D("3.0")], [None, None, None]],
            # we have to specify precision, because `AnonymousListBuilder::finish`
            # use `ArrowDataType` which will remap `None` precision to `38`
            pl.Array(pl.Decimal(precision=38, scale=1), shape=3),
        ),
    ],
)
def test_df_serde_array(data: Any, dtype: pl.DataType) -> None:
    df = pl.DataFrame({"foo": data}, schema={"foo": dtype})
    buf = io.BytesIO()
    df.serialize(buf)
    buf.seek(0)
    deserialized_df = pl.DataFrame.deserialize(buf)
    assert_frame_equal(deserialized_df, df)


@pytest.mark.parametrize(
    ("data", "dtype"),
    [
        (
            [
                [
                    datetime(1997, 10, 1),
                    datetime(2000, 1, 2, 10, 30, 1),
                ],
                [None, None],
            ],
            pl.Array(pl.Datetime, shape=2),
        ),
        (
            [[date(1997, 10, 1), date(2000, 1, 1)], [None, None]],
            pl.Array(pl.Date, shape=2),
        ),
        (
            [
                [timedelta(seconds=1), timedelta(seconds=10)],
                [None, None],
            ],
            pl.Array(pl.Duration, shape=2),
        ),
    ],
)
def test_df_serde_array_logical_inner_type(data: Any, dtype: pl.DataType) -> None:
    df = pl.DataFrame({"foo": data}, schema={"foo": dtype})
    buf = io.BytesIO()
    df.serialize(buf)
    buf.seek(0)
    result = pl.DataFrame.deserialize(buf)
    assert_frame_equal(result, df)


def test_df_serde_float_inf_nan() -> None:
    df = pl.DataFrame({"a": [1.0, float("inf"), float("-inf"), float("nan")]})
    ser = df.serialize(format="json")
    result = pl.DataFrame.deserialize(io.StringIO(ser), format="json")
    assert_frame_equal(result, df)


def test_df_serialize_invalid_type() -> None:
    df = pl.DataFrame({"a": [object()]})
    with pytest.raises(
        ComputeError, match="serializing data of type Object is not supported"
    ):
        df.serialize()


def test_df_serde_list_of_null_17230() -> None:
    df = pl.Series([[]], dtype=pl.List(pl.Null)).to_frame()
    ser = df.serialize(format="json")
    result = pl.DataFrame.deserialize(io.StringIO(ser), format="json")
    assert_frame_equal(result, df)


def test_df_serialize_from_multiple_python_threads_22364() -> None:
    df = pl.DataFrame({"A": [1, 2, 3, 4]})

    with ThreadPool(4) as tp:
        tp.map(pickle.dumps, [df] * 1_000)
