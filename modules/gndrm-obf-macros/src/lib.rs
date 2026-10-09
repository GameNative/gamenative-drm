//! `obf!` / `obfc!` — compile-time string obfuscation.
//!
//! `obf!("...")` expands to a stack-decoded string value ([`gndrm_obf::Obf`]).
//! `obf!("literal {arg:spec}", args...)` additionally reproduces `format_args!`
//! semantics: the literal text is decoded at runtime and the arguments are
//! formatted in place, so the message never lands in `.rodata`.
//! `obfc!("...")` produces a NUL-terminated decoded value for C string use.
//!
//! Load-bearing strings (env names, Vulkan names, socket names, runtime-compared
//! paths) are deliberately left as plain literals by the call sites.

use proc_macro::TokenStream;
use proc_macro2::Literal;
use quote::quote;
use syn::{
    parse::{Parse, ParseStream},
    parse_macro_input, Expr, LitStr, Token,
};

/// Parse `"fmt", arg, arg, ...`.
struct Input {
    fmt: LitStr,
    args: Vec<Expr>,
}

impl Parse for Input {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let fmt: LitStr = input.parse()?;
        let mut args = Vec::new();
        while input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            args.push(input.parse::<Expr>()?);
        }
        Ok(Input { fmt, args })
    }
}

/// A parsed format string: literal runs and placeholders, in order.
enum Seg {
    Text(String),
    Place { name: Option<String>, spec: String },
}

/// Split a Rust format string into text runs and placeholders.
fn parse_fmt(s: &str) -> syn::Result<Vec<Seg>> {
    let mut segs = Vec::new();
    let mut text = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '{' => {
                if it.peek() == Some(&'{') {
                    it.next();
                    text.push('{');
                    continue;
                }
                if !text.is_empty() {
                    segs.push(Seg::Text(std::mem::take(&mut text)));
                }
                let mut inner = String::new();
                loop {
                    match it.next() {
                        Some('}') => break,
                        Some('{') => {
                            return Err(syn::Error::new(
                                proc_macro2::Span::call_site(),
                                "obf!: nested format placeholders are unsupported",
                            ))
                        }
                        Some(ch) => inner.push(ch),
                        None => {
                            return Err(syn::Error::new(
                                proc_macro2::Span::call_site(),
                                "obf!: unterminated format placeholder",
                            ))
                        }
                    }
                }
                let (name, spec) = match inner.split_once(':') {
                    Some((n, sp)) => (n.to_string(), format!(":{sp}")),
                    None => (inner, String::new()),
                };
                let name = if name.is_empty() { None } else { Some(name) };
                segs.push(Seg::Place { name, spec });
            }
            '}' => {
                if it.peek() == Some(&'}') {
                    it.next();
                }
                text.push('}');
            }
            other => text.push(other),
        }
    }
    if !text.is_empty() {
        segs.push(Seg::Text(text));
    }
    Ok(segs)
}

/// Per-literal XOR key: FNV-1a over the bytes, low 8 bits.
fn key_for(bytes: &[u8]) -> u8 {
    let mut h: u32 = 0x811c_dc35;
    for &b in bytes {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h as u8
}

/// Encode `bytes` under `key` (inverse of `gndrm_obf::Obf::new`).
fn encode(bytes: &[u8], key: u8) -> Vec<u8> {
    bytes
        .iter()
        .enumerate()
        .map(|(i, &b)| b ^ key.wrapping_add(i as u8))
        .collect()
}

/// Emit `::gndrm_obf::Obf::<N>::new([..], K)` for `text`.
fn obf_tokens(text: &str) -> proc_macro2::TokenStream {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let key = key_for(bytes);
    let enc = encode(bytes, key);
    let elems = enc.iter().map(|b| Literal::u8_unsuffixed(*b));
    quote! { ::gndrm_obf::Obf::<#n>::new([#(#elems),*], #key) }
}

/// `obfc!("literal")` — a NUL-terminated decoded C string.
#[proc_macro]
pub fn obfc(input: TokenStream) -> TokenStream {
    let lit = parse_macro_input!(input as LitStr);
    let mut bytes = lit.value().into_bytes();
    bytes.push(0);
    let n = bytes.len();
    let key = key_for(&bytes);
    let enc = encode(&bytes, key);
    let elems = enc.iter().map(|b| Literal::u8_unsuffixed(*b));
    quote! { ::gndrm_obf::ObfC::<#n>::new([#(#elems),*], #key) }.into()
}

/// `obf!("literal {arg:spec}", args...)`.
#[proc_macro]
pub fn obf(input: TokenStream) -> TokenStream {
    let Input { fmt, args } = parse_macro_input!(input as Input);
    let segs = match parse_fmt(&fmt.value()) {
        Ok(s) => s,
        Err(e) => return e.to_compile_error().into(),
    };

    // Named args (`name = expr`), if any; the rest stay positional. Expressions
    // are carried as token streams (syn::Expr is not `Clone`).
    let mut named: Vec<(String, proc_macro2::TokenStream)> = Vec::new();
    let mut positional: Vec<proc_macro2::TokenStream> = Vec::new();
    for a in args {
        match a {
            Expr::Assign(asn) => {
                if let Expr::Path(p) = asn.left.as_ref() {
                    if let Some(id) = p.path.get_ident() {
                        named.push((id.to_string(), quote::ToTokens::to_token_stream(&asn.right)));
                        continue;
                    }
                }
                positional.push(quote::ToTokens::to_token_stream(&Expr::Assign(asn)));
            }
            other => positional.push(quote::ToTokens::to_token_stream(&other)),
        }
    }

    let mut fmt_out = String::new();
    let mut emit: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut next_pos = 0usize;
    let mut saw_place = false;

    for seg in segs {
        match seg {
            Seg::Text(t) => {
                fmt_out.push_str("{}");
                emit.push(obf_tokens(&t));
            }
            Seg::Place { name, spec } => {
                saw_place = true;
                fmt_out.push('{');
                fmt_out.push_str(&spec);
                fmt_out.push('}');
                let expr: proc_macro2::TokenStream = match &name {
                    None => {
                        if next_pos >= positional.len() {
                            return syn::Error::new(fmt.span(), "obf!: missing format argument")
                                .to_compile_error()
                                .into();
                        }
                        let e = positional[next_pos].clone();
                        next_pos += 1;
                        e
                    }
                    Some(n) => {
                        if let Ok(idx) = n.parse::<usize>() {
                            match positional.get(idx) {
                                Some(e) => e.clone(),
                                None => {
                                    return syn::Error::new(
                                        fmt.span(),
                                        "obf!: positional index out of range",
                                    )
                                    .to_compile_error()
                                    .into();
                                }
                            }
                        } else if let Some((_, e)) = named.iter().find(|(nm, _)| nm == n) {
                            e.clone()
                        } else {
                            // Captured identifier: re-emit the name as a value.
                            match syn::parse_str::<Expr>(n) {
                                Ok(e) => quote::ToTokens::to_token_stream(&e),
                                Err(err) => return err.to_compile_error().into(),
                            }
                        }
                    }
                };
                emit.push(expr);
            }
        }
    }

    // Pure literal — no formatting needed.
    if !saw_place {
        if let Some(only) = emit.into_iter().next() {
            return only.into();
        }
        return syn::Error::new(fmt.span(), "obf!: empty literal")
            .to_compile_error()
            .into();
    }

    let fmt_lit = LitStr::new(&fmt_out, fmt.span());
    quote! {
        ::gndrm_obf::fmt_with(|__f| ::core::write!(__f, #fmt_lit, #(#emit),*))
    }
    .into()
}
