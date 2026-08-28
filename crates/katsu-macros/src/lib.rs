//! Procedural macros for katsu.
//!
//! `#[katsu::export]` puts a Rust function on a JavaScript object with no FFI boundary,
//! no C ABI and no serialization, because the runtime and the exported function are in the
//! same Rust program. That is the structural advantage over napi-rs and it is what
//! `spec/11-rust-interop.md` is about.

use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemFn, parse_macro_input};

/// Export a Rust function to JavaScript.
///
/// ```ignore
/// #[katsu::export]
/// fn add(a: f64, b: f64) -> f64 {
///     a + b
/// }
/// ```
///
/// The generated glue is being built out in M10. For now the attribute validates that it
/// is applied to a function and emits it unchanged, so that code written against the final
/// shape compiles today and starts working when the glue lands.
#[proc_macro_attribute]
pub fn export(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    quote! { #function }.into()
}
