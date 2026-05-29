# BSX v2 Wire Format

## File layout

```
[0..32]             Header (32 bytes)
[32..blob_end]      Asset data (packed, XOR-obfuscated for binmap mode)
[blob_end..toc_off] Audio tracks (RawOpus payloads)
[toc_off..]         TOC (zstd-compressed, XOR-obfuscated, bincode-serialized)
```

## Header (32 bytes)

| Offset | Size | Field      | Value                        |
|--------|------|------------|------------------------------|
| 0      | 4    | magic      | `BSX\x01`                    |
| 4      | 2    | version    | `2` LE                       |
| 6      | 2    | _reserved_ | `0`                          |
| 8      | 8    | toc_offset | absolute byte offset of TOC  |
| 16     | 4    | toc_size   | compressed TOC byte count    |
| 20     | 4    | _reserved_ | `0`                          |
| 24     | 4    | header_crc | CRC32 of bytes 0..24         |
| 28     | 4    | _reserved_ | `0`                          |

## TOC

TOC bytes at `toc_offset` are:

1. XOR'd with a 16-byte key (assembled at runtime)
2. zstd-compressed (magic stripped)
3. bincode-serialized `BsxToc` struct

## RawOpus v2

```
[0]     version     u8 = 2
[1]     channels    u8
[2..4]  pre_skip    u16 LE
[4..8]  num_packets u32 LE
[8]     cbr_flag    u8  (1 = single VarInt; 0 = N VarInts)
[9..]   lengths     LEB128 VarInts
[...]   raw Opus frame data
```

`get()` rewraps to a valid Ogg Opus container automatically.

## Binmap mode (`--binmap`)

All assets concatenated into a single flat blob, XOR-obfuscated with a
per-byte key derived from position. TOC contains byte offsets.

## `--fs` mode

`BsxToc.fs_map` maps `safe_key → original_path`.  
Safe keys: directory components lowercased, non-alphanumeric → `_`.

## Obfuscation

XOR keys are assembled from obfuscated constants at call-time. No plaintext
key sequences appear in the compiled binary.
