//! Compiler plugin for Rust-PHF
//!
//! See the documentation for the `phf` crate for more details.
#![crate_id="github.com/sfackler/rust-phf/phf_mac"]
#![crate_type="dylib"]
#![doc(html_root_url="http://www.rust-ci.org/sfackler/rust-phf/doc")]
#![feature(managed_boxes, macro_registrar, quote)]

extern crate collections;
extern crate rand;
extern crate syntax;
extern crate time;
extern crate phf;

use collections::HashMap;
use std::os;
use syntax::ast;
use syntax::ast::{Name, TokenTree, LitStr, Expr, ExprVec, ExprLit};
use syntax::codemap::Span;
use syntax::ext::base::{SyntaxExtension,
                        ExtCtxt,
                        MacResult,
                        MRExpr,
                        NormalTT,
                        BasicMacroExpander};
use syntax::parse;
use syntax::parse::token;
use syntax::parse::token::{InternedString, COMMA, EOF, FAT_ARROW};
use rand::{Rng, SeedableRng, XorShiftRng};

static DEFAULT_LAMBDA: uint = 5;

static FIXED_SEED: [u32, ..4] = [3141592653, 589793238, 462643383, 2795028841];

#[macro_registrar]
#[doc(hidden)]
pub fn macro_registrar(register: |Name, SyntaxExtension|) {
    let reg = |name, fn_| {
        register(token::intern(name),
                 NormalTT(~BasicMacroExpander {
                     expander: fn_,
                     span: None
                 },
                 None));
    };
    reg("phf_map", expand_phf_map);
    reg("phf_set", expand_phf_set);
    reg("phf_ordered_map", expand_phf_ordered_map);
    reg("phf_ordered_set", expand_phf_ordered_set);
}

struct Entry {
    key_str: InternedString,
    key: @Expr,
    value: @Expr
}

struct HashState {
    k1: u64,
    k2: u64,
    disps: Vec<(uint, uint)>,
    map: Vec<uint>,
}

fn expand_phf_map(cx: &mut ExtCtxt, sp: Span, tts: &[TokenTree]) -> MacResult {
    let entries = match parse_map(cx, tts) {
        Some(entries) => entries,
        None => return MacResult::dummy_expr(sp)
    };

    if has_duplicates(cx, sp, entries.as_slice()) {
        return MacResult::dummy_expr(sp);
    }

    let state = generate_hash(cx, sp, entries.as_slice());

    create_map(cx, sp, entries, state)
}

fn expand_phf_set(cx: &mut ExtCtxt, sp: Span, tts: &[TokenTree]) -> MacResult {
    let entries = match parse_set(cx, tts) {
        Some(entries) => entries,
        None => return MacResult::dummy_expr(sp)
    };

    if has_duplicates(cx, sp, entries.as_slice()) {
        return MacResult::dummy_expr(sp);
    }

    let state = generate_hash(cx, sp, entries.as_slice());

    create_set(cx, sp, entries, state)
}

fn expand_phf_ordered_map(cx: &mut ExtCtxt, sp: Span, tts: &[TokenTree])
                          -> MacResult {
    let entries = match parse_map(cx, tts) {
        Some(entries) => entries,
        None => return MacResult::dummy_expr(sp),
    };

    if has_duplicates(cx, sp, entries.as_slice()) {
        return MacResult::dummy_expr(sp);
    }

    let state = generate_hash(cx, sp, entries.as_slice());

    create_ordered_map(cx, sp, entries, state)
}

fn expand_phf_ordered_set(cx: &mut ExtCtxt, sp: Span, tts: &[TokenTree])
                          -> MacResult {
    let entries = match parse_set(cx, tts) {
        Some(entries) => entries,
        None => return MacResult::dummy_expr(sp)
    };

    if has_duplicates(cx, sp, entries.as_slice()) {
        return MacResult::dummy_expr(sp);
    }

    let state = generate_hash(cx, sp, entries.as_slice());

    create_ordered_set(cx, sp, entries, state)
}

fn parse_map(cx: &mut ExtCtxt, tts: &[TokenTree]) -> Option<Vec<Entry>> {
    let mut parser = parse::new_parser_from_tts(cx.parse_sess(), cx.cfg(),
                                                Vec::from_slice(tts));
    let mut entries = Vec::new();

    let mut bad = false;
    while parser.token != EOF {
        let key = cx.expand_expr(parser.parse_expr());
        let key_str = parse_str(cx, key).unwrap_or_else(|| {
            bad = true;
            InternedString::new("")
        });

        if !parser.eat(&FAT_ARROW) {
            cx.span_err(parser.span, "expected `=>`");
            return None;
        }

        let value = parser.parse_expr();

        entries.push(Entry {
            key_str: key_str,
            key: key,
            value: value
        });

        if !parser.eat(&COMMA) && parser.token != EOF {
            cx.span_err(parser.span, "expected `,`");
            return None;
        }
    }

    if entries.len() > phf::MAX_SIZE {
        cx.span_err(parser.span,
                    format!("maps with more than {} entries are not supported",
                            phf::MAX_SIZE));
        return None;
    }

    if bad {
        return None;
    }

    Some(entries)
}

fn parse_set(cx: &mut ExtCtxt, tts: &[TokenTree]) -> Option<Vec<Entry>> {
    let mut parser = parse::new_parser_from_tts(cx.parse_sess(), cx.cfg(),
                                                Vec::from_slice(tts));
    let mut entries = Vec::new();
    let value = quote_expr!(&*cx, ());

    let mut bad = false;
    while parser.token != EOF {
        let key = cx.expand_expr(parser.parse_expr());
        let key_str = parse_str(cx, key).unwrap_or_else(|| {
            bad = true;
            InternedString::new("")
        });

        entries.push(Entry {
            key_str: key_str,
            key: key,
            value: value,
        });

        if !parser.eat(&COMMA) && parser.token != EOF {
            cx.span_err(parser.span, "expected `,`");
            return None;
        }
    }

    if entries.len() > phf::MAX_SIZE {
        cx.span_err(parser.span,
                    format!("maps with more than {} entries are not supported",
                            phf::MAX_SIZE));
        return None;
    }

    if bad {
        return None;
    }

    Some(entries)
}

fn parse_str(cx: &mut ExtCtxt, e: &Expr) -> Option<InternedString> {
    match e.node {
        ExprLit(lit) => {
            match lit.node {
                LitStr(ref s, _) => Some(s.clone()),
                _ => {
                    cx.span_err(e.span, "expected string literal");
                    None
                }
            }
        }
        _ => {
            cx.span_err(e.span, "expected string literal");
            None
        }
    }
}

fn has_duplicates(cx: &mut ExtCtxt, sp: Span, entries: &[Entry]) -> bool {
    let mut dups = false;
    let mut strings = HashMap::new();
    for entry in entries.iter() {
        strings.insert_or_update_with(entry.key_str.clone(), (entry, true),
                                      |_, &(orig, ref mut first)| {
            if *first {
                cx.span_err(sp, format!("duplicate key \"{}\"",
                                        entry.key_str));
                cx.span_note(orig.key.span, "one occurrence here");
                *first = false;
            }
            cx.span_note(entry.key.span, "one occurrence here");
            dups = true;
        });
    }

    dups
}

fn generate_hash(cx: &mut ExtCtxt, sp: Span, entries: &[Entry]) -> HashState {
    let mut rng: XorShiftRng = SeedableRng::from_seed(FIXED_SEED);
    let start = time::precise_time_s();
    let state;
    loop {
        match try_generate_hash(entries.as_slice(), &mut rng) {
            Some(s) => {
                state = s;
                break;
            }
            None => {}
        }
    }
    let time = time::precise_time_s() - start;
    if os::getenv("PHF_STATS").is_some() {
        cx.span_note(sp, format!("PHF generation took {} seconds", time));
    }

    state
}

fn try_generate_hash(entries: &[Entry], rng: &mut XorShiftRng)
                     -> Option<HashState> {
    struct Bucket {
        idx: uint,
        keys: Vec<uint>,
    }

    struct Hashes {
        g: uint,
        f1: uint,
        f2: uint,
    }

    let k1 = rng.gen();
    let k2 = rng.gen();

    let hashes: Vec<Hashes> = entries.iter().map(|entry| {
        let (g, f1, f2) = phf::hash(entry.key_str.get(), k1, k2);
        Hashes {
            g: g,
            f1: f1,
            f2: f2
        }
    }).collect();

    let buckets_len = (entries.len() + DEFAULT_LAMBDA - 1) / DEFAULT_LAMBDA;
    let mut buckets = Vec::from_fn(buckets_len,
                                   |i| Bucket { idx: i, keys: Vec::new() });

    for (i, hash) in hashes.iter().enumerate() {
        buckets.get_mut(hash.g % buckets_len).keys.push(i);
    }

    // Sort descending
    buckets.sort_by(|a, b| b.keys.len().cmp(&a.keys.len()));

    let table_len = entries.len();
    let mut map = Vec::from_elem(table_len, None);
    let mut disps = Vec::from_elem(buckets_len, (0u, 0u));
    let mut try_map = HashMap::new();
    'buckets: for bucket in buckets.iter() {
        for d1 in range(0, table_len) {
            'disps_l: for d2 in range(0, table_len) {
                try_map.clear();
                for &key in bucket.keys.iter() {
                    let idx = phf::displace(hashes.get(key).f1,
                                            hashes.get(key).f2,
                                            d1,
                                            d2) % table_len;
                    if map.get(idx).is_some() || try_map.find(&idx).is_some() {
                        continue 'disps_l;
                    }
                    try_map.insert(idx, key);
                }

                // We've picked a good set of disps
                *disps.get_mut(bucket.idx) = (d1, d2);
                for (&idx, &key) in try_map.iter() {
                    *map.get_mut(idx) = Some(key);
                }
                continue 'buckets;
            }
        }

        // Unable to find displacements for a bucket
        return None;
    }

    Some(HashState {
        k1: k1,
        k2: k2,
        disps: disps,
        map: map.move_iter().map(|i| i.unwrap()).collect(),
    })
}

fn create_map(cx: &mut ExtCtxt, sp: Span, entries: Vec<Entry>, state: HashState)
              -> MacResult {
    let disps = state.disps.iter().map(|&(d1, d2)| {
        quote_expr!(&*cx, ($d1, $d2))
    }).collect();
    let disps = create_slice_expr(disps, sp);

    let entries = state.map.iter().map(|&idx| {
        let &Entry { key, value, .. } = entries.get(idx);
        quote_expr!(&*cx, ($key, $value))
    }).collect();
    let entries = create_slice_expr(entries, sp);

    let k1 = state.k1;
    let k2 = state.k2;
    MRExpr(quote_expr!(cx, PhfMap {
        k1: $k1,
        k2: $k2,
        disps: &'static $disps,
        entries: &'static $entries,
    }))
}

fn create_set(cx: &mut ExtCtxt, sp: Span, entries: Vec<Entry>, state: HashState)
              -> MacResult {
    let map = match create_map(cx, sp, entries, state) {
        MRExpr(expr) => expr,
        _ => unreachable!(),
    };

    MRExpr(quote_expr!(cx, PhfSet { map: $map }))
}

fn create_ordered_map(cx: &mut ExtCtxt, sp: Span, entries: Vec<Entry>,
                      state: HashState) -> MacResult {
    let disps = state.disps.iter().map(|&(d1, d2)| {
        quote_expr!(&*cx, ($d1, $d2))
    }).collect();
    let disps = create_slice_expr(disps, sp);

    let idxs = state.map.iter().map(|&idx| quote_expr!(&*cx, $idx)).collect();
    let idxs = create_slice_expr(idxs, sp);

    let entries = entries.iter().map(|&Entry { key, value, .. }| {
        quote_expr!(&*cx, ($key, $value))
    }).collect();
    let entries = create_slice_expr(entries, sp);

    let k1 = state.k1;
    let k2 = state.k2;
    MRExpr(quote_expr!(cx, PhfOrderedMap {
        k1: $k1,
        k2: $k2,
        disps: &'static $disps,
        idxs: &'static $idxs,
        entries: &'static $entries,
    }))
}

fn create_ordered_set(cx: &mut ExtCtxt, sp: Span, entries: Vec<Entry>,
                      state: HashState) -> MacResult {
    let map = match create_ordered_map(cx, sp, entries, state) {
        MRExpr(expr) => expr,
        _ => unreachable!(),
    };

    MRExpr(quote_expr!(cx, PhfOrderedSet { map: $map }))
}

fn create_slice_expr(vec: Vec<@Expr>, sp: Span) -> @Expr {
    @Expr {
        id: ast::DUMMY_NODE_ID,
        node: ExprVec(vec),
        span: sp
    }
}
