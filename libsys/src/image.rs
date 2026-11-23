//! Reading the library's `.dynsym` from its first segment (the bytes of the mapped image, address 0 being its first byte: the object is linked at 0
//! and its first segment holds the headers, `.dynsym`, `.hash`, `.dynstr`, the code and `.dynamic`).

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const DT_NULL: u64 = 0;
const DT_HASH: u64 = 4;
const DT_STRTAB: u64 = 5;
const DT_SYMTAB: u64 = 6;
const DT_RELA: u64 = 7;
const DT_REL: u64 = 17;
const DT_JMPREL: u64 = 23;
const SYM_SIZE: usize = 24;

/// Why a byte string is not the library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Not an ELF64 little-endian RISC-V shared object, or a table lies outside the segment.
    Malformed,
    /// A relocation table: the library must need none.
    HasRelocations,
}

/// One entry of `.dynsym`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Symbol<'a> {
    pub name: &'a str,
    /// The address, as an offset from the library's base.
    pub value: u64,
    pub size: u64,
}

/// The library's first segment.
#[derive(Clone, Copy)]
pub struct Image<'a> {
    bytes: &'a [u8],
    symtab: usize,
    strtab: usize,
    count: usize,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}
fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        b.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// The program headers of an ELF64 header: `(type, flags, offset, vaddr, filesz, memsz)` of entry `i`.
fn phdr(b: &[u8], i: usize) -> Option<(u32, u32, u64, u64, u64, u64)> {
    let phoff = usize::try_from(u64_at(b, 32)?).ok()?;
    let phentsize = usize::from(u16_at(b, 54)?);
    let at = phoff.checked_add(i.checked_mul(phentsize)?)?;
    Some((
        u32_at(b, at)?,
        u32_at(b, at + 4)?,
        u64_at(b, at + 8)?,
        u64_at(b, at + 16)?,
        u64_at(b, at + 32)?,
        u64_at(b, at + 40)?,
    ))
}

fn check_header(b: &[u8]) -> Result<usize, Error> {
    let ok = b.get(..4) == Some(&[0x7f, b'E', b'L', b'F'])
        && b.get(4) == Some(&2)
        && b.get(5) == Some(&1)
        && u16_at(b, 16) == Some(3)
        && u16_at(b, 18) == Some(243)
        && u16_at(b, 54) == Some(56);
    if !ok {
        return Err(Error::Malformed);
    }
    Ok(usize::from(u16_at(b, 56).ok_or(Error::Malformed)?))
}

/// The size of the first loadable segment, read from the headers at the start of `header` (a mapped page is enough): how many bytes
/// [`Image::new`] wants. `None` when `header` is not the library's.
pub fn image_len(header: &[u8]) -> Option<usize> {
    let n = check_header(header).ok()?;
    (0..n).find_map(|i| match phdr(header, i)? {
        (PT_LOAD, _, 0, 0, filesz, _) => usize::try_from(filesz).ok(),
        _ => None,
    })
}

impl<'a> Image<'a> {
    /// Read the headers and the dynamic section of `bytes`, the first segment.
    pub fn new(bytes: &'a [u8]) -> Result<Image<'a>, Error> {
        let n = check_header(bytes)?;
        let dynamic = (0..n)
            .find_map(|i| match phdr(bytes, i)? {
                (PT_DYNAMIC, _, off, _, size, _) => Some((off, size)),
                _ => None,
            })
            .ok_or(Error::Malformed)?;
        let (mut at, end) = (
            usize::try_from(dynamic.0).map_err(|_| Error::Malformed)?,
            usize::try_from(dynamic.0.saturating_add(dynamic.1)).map_err(|_| Error::Malformed)?,
        );
        let (mut hash, mut strtab, mut symtab) = (None, None, None);
        while at.checked_add(16).is_some_and(|e| e <= end) {
            let tag = u64_at(bytes, at).ok_or(Error::Malformed)?;
            let val = u64_at(bytes, at + 8).ok_or(Error::Malformed)?;
            match tag {
                DT_NULL => break,
                DT_HASH => hash = Some(val),
                DT_STRTAB => strtab = Some(val),
                DT_SYMTAB => symtab = Some(val),
                DT_RELA | DT_REL | DT_JMPREL => return Err(Error::HasRelocations),
                _ => {}
            }
            at += 16;
        }
        let (hash, strtab, symtab) = (
            usize::try_from(hash.ok_or(Error::Malformed)?).map_err(|_| Error::Malformed)?,
            usize::try_from(strtab.ok_or(Error::Malformed)?).map_err(|_| Error::Malformed)?,
            usize::try_from(symtab.ok_or(Error::Malformed)?).map_err(|_| Error::Malformed)?,
        );
        // the SysV hash table's second word is the number of symbols
        let count = usize::try_from(
            u32_at(bytes, hash.checked_add(4).ok_or(Error::Malformed)?).ok_or(Error::Malformed)?,
        )
        .map_err(|_| Error::Malformed)?;
        if symtab
            .checked_add(count.checked_mul(SYM_SIZE).ok_or(Error::Malformed)?)
            .is_none_or(|e| e > bytes.len())
            || strtab > bytes.len()
        {
            return Err(Error::Malformed);
        }
        Ok(Image {
            bytes,
            symtab,
            strtab,
            count,
        })
    }

    /// Every defined symbol, in table order.
    pub fn symbols(&self) -> impl Iterator<Item = Symbol<'a>> + '_ {
        (1..self.count).filter_map(move |i| {
            let at = self.symtab + i * SYM_SIZE;
            let name_at = self.strtab.checked_add(u32_at(self.bytes, at)? as usize)?;
            let tail = self.bytes.get(name_at..)?;
            let len = tail.iter().position(|&c| c == 0)?;
            let name = core::str::from_utf8(&tail[..len]).ok()?;
            let shndx = u16_at(self.bytes, at + 6)?;
            if shndx == 0 {
                return None; // undefined
            }
            Some(Symbol {
                name,
                value: u64_at(self.bytes, at + 8)?,
                size: u64_at(self.bytes, at + 16)?,
            })
        })
    }

    /// The symbol called `name`.
    pub fn lookup(&self, name: &str) -> Option<Symbol<'a>> {
        self.symbols().find(|s| s.name == name)
    }
}
