// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod range_fn;

use proc_macro::TokenStream;
use quote::{quote, quote_spanned};
use range_fn::process_range_fn;
use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::{parse_macro_input, DeriveInput, ItemStruct};

/// Make struct implemented trait [AggrFuncTypeStore], which is necessary when writing UDAF.
/// This derive macro is expect to be used along with attribute macro [as_aggr_func_creator].
#[proc_macro_derive(AggrFuncTypeStore)]
pub fn aggr_func_type_store_derive(input: TokenStream) -> TokenStream {
    let ast = parse_macro_input!(input as DeriveInput);
    impl_aggr_func_type_store(&ast)
}

fn impl_aggr_func_type_store(ast: &DeriveInput) -> TokenStream {
    let name = &ast.ident;
    let gen = quote! {
        use common_query::logical_plan::accumulator::AggrFuncTypeStore;
        use common_query::error::{InvalidInputStateSnafu, Error as QueryError};
        use datatypes::prelude::ConcreteDataType;

        impl AggrFuncTypeStore for #name {
            fn input_types(&self) -> std::result::Result<Vec<ConcreteDataType>, QueryError> {
                let input_types = self.input_types.load();
                snafu::ensure!(input_types.is_some(), InvalidInputStateSnafu);
                Ok(input_types.as_ref().unwrap().as_ref().clone())
            }

            fn set_input_types(&self, input_types: Vec<ConcreteDataType>) -> std::result::Result<(), QueryError> {
                let old = self.input_types.swap(Some(std::sync::Arc::new(input_types.clone())));
                if let Some(old) = old {
                    snafu::ensure!(old.len() == input_types.len(), InvalidInputStateSnafu);
                    for (x, y) in old.iter().zip(input_types.iter()) {
                        snafu::ensure!(x == y, InvalidInputStateSnafu);
                    }
                }
                Ok(())
            }
        }
    };
    gen.into()
}

/// A struct can be used as a creator for aggregate function if it has been annotated with this
/// attribute first. This attribute add a necessary field which is intended to store the input
/// data's types to the struct.
/// This attribute is expected to be used along with derive macro [AggrFuncTypeStore].
#[proc_macro_attribute]
pub fn as_aggr_func_creator(_args: TokenStream, input: TokenStream) -> TokenStream {
    let mut item_struct = parse_macro_input!(input as ItemStruct);
    if let syn::Fields::Named(ref mut fields) = item_struct.fields {
        let result = syn::Field::parse_named.parse2(quote! {
            input_types: arc_swap::ArcSwapOption<Vec<ConcreteDataType>>
        });
        match result {
            Ok(field) => fields.named.push(field),
            Err(e) => return e.into_compile_error().into(),
        }
    } else {
        return quote_spanned!(
            item_struct.fields.span() => compile_error!(
                "This attribute macro needs to add fields to the its annotated struct, \
                so the struct must have \"{}\".")
        )
        .into();
    }
    quote! {
        #item_struct
    }
    .into()
}

/// Attribute macro to convert an arithimetic function to a range function. The annotated function
/// should accept servaral arrays as input and return a single value as output. This procedure
/// macro can works on any number of input parameters. Return type can be either primitive type
/// or wrapped in `Option`.
///
/// # Example
/// Take `count_over_time()` in PromQL as an example:
/// ```rust, ignore
/// /// The count of all values in the specified interval.
/// #[range_fn(
///     name = "CountOverTime",
///     ret = "Float64Array",
///     display_name = "prom_count_over_time"
/// )]
/// pub fn count_over_time(_: &TimestampMillisecondArray, values: &Float64Array) -> f64 {
///      values.len() as f64
/// }
/// ```
///
/// # Arguments
/// - `name`: The name of the generated [ScalarUDF] struct.
/// - `ret`: The return type of the generated UDF function.
/// - `display_name`: The display name of the generated UDF function.
#[proc_macro_attribute]
pub fn range_fn(args: TokenStream, input: TokenStream) -> TokenStream {
    process_range_fn(args, input)
}
