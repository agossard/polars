mod count;
mod dsl;
#[cfg(feature = "python")]
mod python_udf;
mod schema;

use std::borrow::Cow;
use std::fmt::{Debug, Display, Formatter};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

pub use dsl::*;
use polars_core::error::feature_gated;
use polars_core::prelude::*;
use polars_io::cloud::CloudOptions;
use polars_utils::pl_str::PlSmallStr;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use strum_macros::IntoStaticStr;

#[cfg(feature = "python")]
use crate::dsl::python_dsl::PythonFunction;
use crate::plans::ir::ScanSourcesDisplay;
use crate::prelude::*;

#[cfg_attr(feature = "ir_serde", derive(Serialize, Deserialize))]
#[derive(Clone, IntoStaticStr)]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE")]
pub enum FunctionIR {
    RowIndex {
        name: PlSmallStr,
        offset: Option<IdxSize>,
        // Might be cached.
        #[cfg_attr(feature = "ir_serde", serde(skip))]
        schema: CachedSchema,
    },
    #[cfg(feature = "python")]
    OpaquePython(OpaquePythonUdf),

    FastCount {
        sources: ScanSources,
        scan_type: Box<FileScanIR>,
        cloud_options: Option<CloudOptions>,
        alias: Option<PlSmallStr>,
    },

    Unnest {
        columns: Arc<[PlSmallStr]>,
    },
    Rechunk,
    Explode {
        columns: Arc<[PlSmallStr]>,
        #[cfg_attr(feature = "ir_serde", serde(skip))]
        schema: CachedSchema,
    },
    #[cfg(feature = "pivot")]
    Unpivot {
        args: Arc<UnpivotArgsIR>,
        #[cfg_attr(feature = "ir_serde", serde(skip))]
        schema: CachedSchema,
    },
    #[cfg_attr(feature = "ir_serde", serde(skip))]
    Opaque {
        function: Arc<dyn DataFrameUdf>,
        schema: Option<Arc<dyn UdfSchema>>,
        ///  allow predicate pushdown optimizations
        predicate_pd: bool,
        ///  allow projection pushdown optimizations
        projection_pd: bool,
        streamable: bool,
        // used for formatting
        fmt_str: PlSmallStr,
    },
}

impl Eq for FunctionIR {}

impl PartialEq for FunctionIR {
    fn eq(&self, other: &Self) -> bool {
        use FunctionIR::*;
        match (self, other) {
            (Rechunk, Rechunk) => true,
            (
                FastCount {
                    sources: srcs_l, ..
                },
                FastCount {
                    sources: srcs_r, ..
                },
            ) => srcs_l == srcs_r,
            (Explode { columns: l, .. }, Explode { columns: r, .. }) => l == r,
            #[cfg(feature = "pivot")]
            (Unpivot { args: l, .. }, Unpivot { args: r, .. }) => l == r,
            (RowIndex { name: l, .. }, RowIndex { name: r, .. }) => l == r,
            _ => false,
        }
    }
}

impl Hash for FunctionIR {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            #[cfg(feature = "python")]
            FunctionIR::OpaquePython { .. } => {},
            FunctionIR::Opaque { fmt_str, .. } => fmt_str.hash(state),
            FunctionIR::FastCount {
                sources,
                scan_type,
                cloud_options,
                alias,
            } => {
                sources.hash(state);
                scan_type.hash(state);
                cloud_options.hash(state);
                alias.hash(state);
            },
            FunctionIR::Unnest { columns } => columns.hash(state),
            FunctionIR::Rechunk => {},
            FunctionIR::Explode { columns, schema: _ } => columns.hash(state),
            #[cfg(feature = "pivot")]
            FunctionIR::Unpivot { args, schema: _ } => args.hash(state),
            FunctionIR::RowIndex {
                name,
                schema: _,
                offset,
            } => {
                name.hash(state);
                offset.hash(state);
            },
        }
    }
}

impl FunctionIR {
    /// Whether this function can run on batches of data at a time.
    pub fn is_streamable(&self) -> bool {
        use FunctionIR::*;
        match self {
            Rechunk => false,
            FastCount { .. } | Unnest { .. } | Explode { .. } => true,
            #[cfg(feature = "pivot")]
            Unpivot { .. } => true,
            Opaque { streamable, .. } => *streamable,
            #[cfg(feature = "python")]
            OpaquePython(OpaquePythonUdf { streamable, .. }) => *streamable,
            RowIndex { .. } => false,
        }
    }

    /// Whether this function will increase the number of rows
    pub fn expands_rows(&self) -> bool {
        use FunctionIR::*;
        match self {
            #[cfg(feature = "pivot")]
            Unpivot { .. } => true,
            Explode { .. } => true,
            _ => false,
        }
    }

    pub(crate) fn allow_predicate_pd(&self) -> bool {
        use FunctionIR::*;
        match self {
            Opaque { predicate_pd, .. } => *predicate_pd,
            #[cfg(feature = "python")]
            OpaquePython(OpaquePythonUdf { predicate_pd, .. }) => *predicate_pd,
            #[cfg(feature = "pivot")]
            Unpivot { .. } => true,
            Rechunk | Unnest { .. } | Explode { .. } => true,
            RowIndex { .. } | FastCount { .. } => false,
        }
    }

    pub(crate) fn allow_projection_pd(&self) -> bool {
        use FunctionIR::*;
        match self {
            Opaque { projection_pd, .. } => *projection_pd,
            #[cfg(feature = "python")]
            OpaquePython(OpaquePythonUdf { projection_pd, .. }) => *projection_pd,
            Rechunk | FastCount { .. } | Unnest { .. } | Explode { .. } => true,
            #[cfg(feature = "pivot")]
            Unpivot { .. } => true,
            RowIndex { .. } => true,
        }
    }

    pub(crate) fn additional_projection_pd_columns(&self) -> Cow<'_, [PlSmallStr]> {
        use FunctionIR::*;
        match self {
            Unnest { columns } => Cow::Borrowed(columns.as_ref()),
            Explode { columns, .. } => Cow::Borrowed(columns.as_ref()),
            _ => Cow::Borrowed(&[]),
        }
    }

    pub fn evaluate(&self, mut df: DataFrame) -> PolarsResult<DataFrame> {
        use FunctionIR::*;
        match self {
            Opaque { function, .. } => function.call_udf(df),
            #[cfg(feature = "python")]
            OpaquePython(OpaquePythonUdf {
                function,
                validate_output,
                schema,
                ..
            }) => python_udf::call_python_udf(function, df, *validate_output, schema.clone()),
            FastCount {
                sources,
                scan_type,
                cloud_options,
                alias,
            } => count::count_rows(sources, scan_type, cloud_options.as_ref(), alias.clone()),
            Rechunk => {
                df.as_single_chunk_par();
                Ok(df)
            },
            Unnest { columns: _columns } => {
                feature_gated!("dtype-struct", df.unnest(_columns.iter().cloned()))
            },
            Explode { columns, .. } => df.explode(columns.iter().cloned()),
            #[cfg(feature = "pivot")]
            Unpivot { args, .. } => {
                use polars_ops::pivot::UnpivotDF;
                let args = (**args).clone();
                df.unpivot2(args)
            },
            RowIndex { name, offset, .. } => df.with_row_index(name.clone(), *offset),
        }
    }
}

impl Debug for FunctionIR {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

impl Display for FunctionIR {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        use FunctionIR::*;
        match self {
            Opaque { fmt_str, .. } => write!(f, "{fmt_str}"),
            Unnest { columns } => {
                write!(f, "UNNEST by:")?;
                let columns = columns.as_ref();
                fmt_column_delimited(f, columns, "[", "]")
            },
            FastCount {
                sources,
                scan_type,
                cloud_options: _,
                alias,
            } => {
                let scan_type: &str = (&(**scan_type)).into();
                let default_column_name = PlSmallStr::from_static(crate::constants::LEN);
                let alias = alias.as_ref().unwrap_or(&default_column_name);

                write!(
                    f,
                    "FAST COUNT ({scan_type}) {} as \"{alias}\"",
                    ScanSourcesDisplay(sources)
                )
            },
            v => {
                let s: &str = v.into();
                write!(f, "{s}")
            },
        }
    }
}
