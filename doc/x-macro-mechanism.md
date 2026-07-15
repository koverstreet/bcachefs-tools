# X-macro import mechanism

bcachefs uses C x-macros extensively for enums, string tables, and type
dispatch. `bch_bindgen/build.rs` has a generic mechanism to import these
into Rust at build time.

## How it works

1. `parse_xmacro(header, "MACRO_NAME")` — parses a C header file, finds
   `#define MACRO_NAME(...)`, and extracts all `x(...)` invocations with
   their arguments. Handles continuation lines and nested parentheses.

2. A `generate_*` function turns the parsed entries into Rust source
   code — enums, match arms, string tables, trait impls, etc.

3. The generated code is written to `OUT_DIR` and pulled in via
   `include!(concat!(env!("OUT_DIR"), "/foo_gen.rs"))`.

## Currently imported x-macros

| X-macro | Source header | Generated output |
|---------|--------------|-----------------|
| `BCH_BKEY_TYPES()` | `bcachefs_format.h` | Typed bkey dispatch enums (`BkeyValI`, `BkeyValSC`, etc.) and accessors |
| `BCH_SB_FIELDS()` | `bcachefs_format.h` | `SbField` trait impls for each superblock field type |
| `BCH_MEMBER_STATES()` | `sb/members_format.h` | Member state string table |
| `BCH_PERSISTENT_COUNTERS()` | `sb/counters_format.h` | Counter info table (name, stable ID, is_sectors flag) |
| `BCH_EXTENT_ENTRY_TYPES()` | `data/extents_format.h` | Extent entry size lookup function |

## Adding a new x-macro import

1. Write a `generate_*` function in `build.rs` that takes `&[Vec<String>]`
   (the parsed entries) and returns a `String` of Rust source code.
2. In `main()`, read the source header, call `parse_xmacro`, and write
   the output to `out_dir.join("your_gen.rs")`.
3. Add `println!("cargo:rerun-if-changed=../path/to/header.h")` so cargo
   rebuilds when the header changes.
4. In the Rust module that needs it:
   `include!(concat!(env!("OUT_DIR"), "/your_gen.rs"));`
