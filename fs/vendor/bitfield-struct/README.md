# Bitfield Struct

[![Crate](https://img.shields.io/crates/v/bitfield-struct.svg)](https://crates.io/crates/bitfield-struct)
[![API](https://docs.rs/bitfield-struct/badge.svg)](https://docs.rs/bitfield-struct)

Procedural macro for bitfields that allows specifying bitfields as structs.
As this library provides a procedural macro, it has no runtime dependencies and works for `no-std` environments.

- Ideal for driver/OS/embedded development (defining HW registers/structures)
- Supports bool flags, integers, and custom types convertible into integers (structs/enums)
- Generates minimalistic, pure, safe rust functions
- Compile-time checks for type and field sizes
- Rust-analyzer/docrs friendly (carries over docs to accessor functions)
- Exports field offsets and sizes as constants (useful for const asserts)
- Optional generation of `Default`, `Clone`, `Debug`, `Hash`, `defmt::Format`, or `binrw::(BinRead|BinWrite)` traits
- Custom internal representation (endianness)
- Auto-generate enum conversion functions `#[bitenum]`
- Supports const generics

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
bitfield-struct = "0.11"
```

## Basics

Let's begin with a simple example.
Suppose we want to store multiple data inside a single Byte, as shown below:

<table>
  <tr>
    <td>7</td>
    <td>6</td>
    <td>5</td>
    <td>4</td>
    <td>3</td>
    <td>2</td>
    <td>1</td>
    <td>0</td>
  </tr>
  <tr>
    <td>P</td>
    <td colspan="2">Level</td>
    <td>S</td>
    <td colspan="4">Kind</td>
  </tr>
</table>

This crate generates a nice wrapper type that makes it easy to do this:

```rust
use bitfield_struct::bitfield;

/// Define your type like this with the bitfield attribute
#[bitfield(u8)]
struct MyByte {
    /// The first field occupies the least significant bits
    #[bits(4)]
    kind: usize,
    /// Booleans are 1 bit large
    system: bool,
    /// The bits attribute specifies the bit size of this field
    #[bits(2)]
    level: usize,
    /// The last field spans over the most significant bits
    present: bool
}
// The macro creates three accessor functions for each field:
// <name>, with_<name> and set_<name>
let my_byte = MyByte::new()
    .with_kind(15)
    .with_system(false)
    .with_level(3)
    .with_present(true);

assert!(my_byte.present());
```

## Features

Additionally, this crate has a few useful features, which are shown here in more detail.

The example below shows how attributes are carried over and how signed integers, padding, and custom types are handled.

```rust
use bitfield_struct::bitfield;

/// A test bitfield with documentation
#[bitfield(u64)]
#[derive(PartialEq, Eq)] // <- Attributes after `bitfield` are carried over
struct MyBitfield {
    /// Defaults to 16 bits for u16
    int: u16,
    /// Interpreted as 1 bit flag, with a custom default value
    #[bits(default = true)]
    flag: bool,
    /// Custom bit size
    #[bits(1)]
    tiny: u8,
    /// Sign extend for signed integers
    #[bits(13)]
    negative: i16,
    /// Supports any type with `into_bits`/`from_bits` functions
    #[bits(16)]
    custom: CustomEnum,
    /// Public field -> public accessor functions
    #[bits(10)]
    pub public: usize,
    /// Also supports read-only fields
    #[bits(1, access = RO)]
    read_only: bool,
    /// And write-only fields
    #[bits(1, access = WO)]
    write_only: bool,
    /// Padding
    #[bits(5)]
    __: u8,
}

/// A custom enum
#[derive(Debug, PartialEq, Eq)]
#[repr(u16)]
enum CustomEnum {
    A = 0,
    B = 1,
    C = 2,
}
impl CustomEnum {
    // This has to be a const fn
    const fn into_bits(self) -> u16 {
        self as _
    }
    const fn from_bits(value: u16) -> Self {
        match value {
            0 => Self::A,
            1 => Self::B,
            _ => Self::C,
        }
    }
}

// Usage:
let mut val = MyBitfield::new()
    .with_int(3 << 15)
    .with_tiny(1)
    .with_negative(-3)
    .with_custom(CustomEnum::B)
    .with_public(2)
    // .with_read_only(true) <- Would not compile
    .with_write_only(false);

println!("{val:?}");
let raw: u64 = val.into();
println!("{raw:b}");

assert_eq!(val.int(), 3 << 15);
assert_eq!(val.flag(), true);
assert_eq!(val.negative(), -3);
assert_eq!(val.tiny(), 1);
assert_eq!(val.custom(), CustomEnum::B);
assert_eq!(val.public(), 2);
assert_eq!(val.read_only(), false);

// const members
assert_eq!(MyBitfield::FLAG_BITS, 1);
assert_eq!(MyBitfield::FLAG_OFFSET, 16);

val.set_negative(1);
assert_eq!(val.negative(), 1);
```

The macro generates three accessor functions for each field.
Each accessor also inherits the documentation of its field.

The signatures for `int` are:

```rust
// generated struct
struct MyBitfield(u64);
impl MyBitfield {
    const fn new() -> Self { Self(0) }

    const INT_BITS: usize = 16;
    const INT_OFFSET: usize = 0;

    const fn int(&self) -> u16 { todo!() }

    const fn with_int(self, value: u16) -> Self { todo!() }
    const fn with_int_checked(self, value: u16) -> Result<Self, ()> { todo!() }

    const fn set_int(&mut self, value: u16) { todo!() }
    const fn set_int_checked(&mut self, value: u16) -> Result<(), ()> { todo!() }

    // other field ...
}
// Also generates From<u64>, Into<u64>, Default, and Debug implementations...
```

> Hint: You can use the rust-analyzer "Expand macro recursively" action to view the generated code.

## Custom Types

The macro supports any types that are convertible into the underlying bitfield type.
This can be enums like in the following example or any other struct.

The conversion and default values can be specified with the following `#[bits]` parameters:
- `from`: Function converting from raw bits into the custom type, defaults to `<ty>::from_bits`
- `into`: Function converting from the custom type into raw bits, defaults to `<ty>::into_bits`
- `default`: Custom expression, defaults to calling `<ty>::from_bits(0)`


```rust
use bitfield_struct::bitfield;

#[bitfield(u16)]
#[derive(PartialEq, Eq)]
struct Bits {
    /// Supports any convertible type
    #[bits(8, default = CustomEnum::B, from = CustomEnum::my_from_bits)]
    custom: CustomEnum,
    /// And nested bitfields
    #[bits(8)]
    nested: Nested,
}

#[derive(Debug, PartialEq, Eq)]
#[repr(u8)]
#[bitenum]
enum CustomEnum {
    A = 0,
    B = 1,
    C = 2,
}
// NOTE: Generated by #[bitenum], but you can also write it yourself
impl CustomEnum {
    // This has to be a const fn
    const fn into_bits(self) -> u8 {
        self as _
    }
    const fn my_from_bits(value: u8) -> Self {
        match value {
            0 => Self::A,
            1 => Self::B,
            _ => Self::C,
        }
    }
}

/// Bitfields implement the conversion functions automatically
#[bitfield(u8)]
struct Nested {
    #[bits(4)]
    lo: u8,
    #[bits(4)]
    hi: u8,
}
```

## Field Order

The optional `order` macro argument determines the layout of the bits, with the default being
Lsb (least significant bit) first:

```rust
use bitfield_struct::bitfield;

#[bitfield(u8, order = Lsb)]
struct MyLsbByte {
    /// The first field occupies the *least* significant bits
    #[bits(4)]
    kind: usize,
    system: bool,
    #[bits(2)]
    level: usize,
    present: bool
}
let my_byte_lsb = MyLsbByte::new()
    .with_kind(10)
    .with_system(false)
    .with_level(2)
    .with_present(true);

//                          .- present
//                          | .- level
//                          | |  .- system
//                          | |  | .- kind
assert_eq!(my_byte_lsb.0, 0b1_10_0_1010);
```

The macro generates the reverse order when Msb (most significant bit) is specified:

```rust
use bitfield_struct::bitfield;

#[bitfield(u8, order = Msb)]
struct MyMsbByte {
    /// The first field occupies the *most* significant bits
    #[bits(4)]
    kind: usize,
    system: bool,
    #[bits(2)]
    level: usize,
    present: bool
}
let my_byte_msb = MyMsbByte::new()
    .with_kind(10)
    .with_system(false)
    .with_level(2)
    .with_present(true);

//                          .- kind
//                          |    .- system
//                          |    | .- level
//                          |    | |  .- present
assert_eq!(my_byte_msb.0, 0b1010_0_10_1);
```

## Custom Representation and Endianness

The macro supports custom types for the representation of the bitfield struct.
This can be an endian-defining type like in the following examples (from [`endian-num`]) or any other struct that can be converted to and from the main bitfield type.

The representation and its conversion functions can be specified with the following `#[bitfield]` parameters:
- `repr` specifies the bitfield's representation in memory
- `from` to specify a conversion function from repr to the bitfield's integer type
- `into` to specify a conversion function from the bitfield's integer type to repr

[`endian-num`]: https://docs.rs/endian-num

This example has a little-endian byte order even on big-endian machines:

```rust
use bitfield_struct::bitfield;
use endian_num::le16;

#[bitfield(u16, repr = le16, from = le16::from_ne, into = le16::to_ne)]
struct MyLeBitfield {
    #[bits(4)]
    first_nibble: u8,
    #[bits(12)]
    other: u16,
}

let my_be_bitfield = MyLeBitfield::new()
    .with_first_nibble(0x1)
    .with_other(0x234);

assert_eq!(my_be_bitfield.into_bits().to_le_bytes(), [0x41, 0x23]);
```

This example has a big-endian byte order even on little-endian machines:

```rust
use bitfield_struct::bitfield;
use endian_num::be16;

#[bitfield(u16, repr = be16, from = be16::from_ne, into = be16::to_ne)]
struct MyBeBitfield {
    #[bits(4)]
    first_nibble: u8,
    #[bits(12)]
    other: u16,
}

let my_be_bitfield = MyBeBitfield::new()
    .with_first_nibble(0x1)
    .with_other(0x234);

assert_eq!(my_be_bitfield.into_bits().to_be_bytes(), [0x23, 0x41]);
```

## Automatic Trait Implementations

### `Clone`, `Copy`
By default, this macro derives `Clone` and `Copy`.
You can disable this with the extra `clone` argument if the semantics of cloning your type require it (e.g. the type holds a pointer to owned data that must also be cloned).
In this case, you can provide your own implementations for `Clone` and `Copy`.

```rust
use bitfield_struct::bitfield;

#[bitfield(u64, clone = false)]
struct CustomClone {
    data: u64
}

impl Clone for CustomClone {
    fn clone(&self) -> Self {
        Self::new().with_data(self.data())
    }
}

// optionally:
impl Copy for CustomClone {}
```

### `fmt::Debug`, `Default`
By default, it also generates suitable `fmt::Debug` and `Default` implementations similar to the ones created for normal structs by `#[derive(Debug, Default)]`.
You can disable this with the extra `debug` and `default` arguments.

```rust
use std::fmt::{Debug, Formatter, Result};
use bitfield_struct::bitfield;

#[bitfield(u64, debug = false, default = false)]
struct CustomDebug {
    data: u64
}
impl Debug for CustomDebug {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        write!(f, "0x{:x}", self.data())
    }
}
impl Default for CustomDebug {
    fn default() -> Self {
        Self(123)
    }
}

let val = CustomDebug::default();
println!("{val:?}")
```

### Support for `defmt::Format`

This macro can automatically implement a `defmt::Format` that mirrors the default `fmt::Debug` implementation by passing the extra `defmt` argument.
This implementation requires the defmt crate to be available as `defmt`, and has the same rules and caveats as `#[derive(defmt::Format)]`.

```rust
use bitfield_struct::bitfield;

#[bitfield(u64, defmt = true)]
struct DefmtExample {
    data: u64
}
```

### Support for `std::hash::Hash`

This macro can also implement `Hash`, which ignores any padding when hashing.

```rust
use bitfield_struct::bitfield;

#[bitfield(u64, hash = true)]
struct HashExample {
    __ignored: u32,
    data: u32,
}
```

### Conditionally Enable `new`/`Clone`/`Debug`/`Default`/`defmt::Format`/`Hash`

Instead of booleans, you can specify `cfg(...)` attributes for `new`, `clone`, `debug`, `default`, `defmt` and `hash`:

```rust
use bitfield_struct::bitfield;

#[bitfield(u64, debug = cfg(test), default = cfg(feature = "foo"))]
struct CustomDebug {
    data: u64
}
```

## Auto-Generate Enum `from_bits`/`into_bits`

Enums are very helpful inside bitfields.
However, manually writing conversion functions is tedious; thus, this crate contains a macro for autogenerating them.

```rust
use bitfield_struct::bitenum;

#[bitenum]
#[repr(u8)]
#[derive(Debug, PartialEq, Eq)]
enum MyEnum {
    #[fallback]
    A = 0,
    B = 1,
    C = 2,
}

assert_eq!(MyEnum::from_bits(1), MyEnum::B);
assert_eq!(MyEnum::C.into_bits(), 2);
```

## Const Generics

Bitfields can use const generics directly. Just declare them on the struct as usual:

```rust
use bitfield_struct::bitfield;

#[bitfield(u16)]
#[derive(PartialEq, Eq)]
struct Packet<const VERSION: u8> {
    #[bits(4)]
    tag: u8,
    #[bits(12)]
    payload: u16,
}
```
