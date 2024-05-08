// Copyright 2023-2024 The Regents of the University of California
// released under BSD 3-Clause License
// author: Kevin Laeufer <laeufer@berkeley.edu>

use crate::fst::{parse_scope_attributes, parse_var_attributes, Attribute};
use crate::hierarchy::*;
use crate::signals::SignalSource;
use crate::viewers::ProgressCount;
use crate::{FileFormat, LoadOptions, TimeTable};
use fst_native::{FstVhdlDataType, FstVhdlVarType};
use num_enum::TryFromPrimitive;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::io::{BufRead, Seek};
use std::sync::atomic::Ordering;

#[derive(Debug, thiserror::Error)]
pub enum VcdParseError {
    #[error("[vcd] failed to parse length: `{0}` for variable `{1}`")]
    VcdVarLengthParsing(String, String),
    #[error("[vcd] failed to parse variable name: `{0}`")]
    VcdVarNameParsing(String),
    #[error("[vcd] expected command to start with `$`, not `{0}`")]
    VcdStartChar(String),
    #[error("[vcd] unexpected number of tokens for command {0}: {1}")]
    VcdUnexpectedNumberOfTokens(String, String),
    #[error("[vcd] encountered a attribute with an unsupported type: {0}")]
    VcdUnsupportedAttributeType(String),
    #[error("[vcd] failed to parse VHDL var type from attribute.")]
    VcdFailedToParseVhdlVarType(
        #[from] num_enum::TryFromPrimitiveError<fst_native::FstVhdlVarType>,
    ),
    #[error("[vcd] failed to parse VHDL data type from attribute.")]
    VcdFailedToParseVhdlDataType(
        #[from] num_enum::TryFromPrimitiveError<fst_native::FstVhdlDataType>,
    ),
    #[error("[vcd] unknown var type: {0}")]
    VcdUnknownVarType(String),
    #[error("[vcd] unknown scope type: {0}")]
    VcdUnknownScopeType(String),
    #[error("failed to decode string")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("failed to parse an integer")]
    ParseInt(#[from] std::num::ParseIntError),
    #[error("I/O operation failed")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, VcdParseError>;

pub fn read_header(
    filename: &str,
    options: &LoadOptions,
) -> Result<(Hierarchy, ReadBodyContinuation, u64)> {
    let input_file = std::fs::File::open(filename)?;
    let mmap = unsafe { memmap2::Mmap::map(&input_file)? };
    let (header_len, hierarchy, lookup) =
        read_hierarchy(&mut std::io::Cursor::new(&mmap[..]), options)?;
    let body_len = (mmap.len() - header_len) as u64;
    let cont = ReadBodyContinuation {
        multi_thread: options.multi_thread,
        header_len,
        lookup,
        input: Input::File(mmap),
    };
    Ok((hierarchy, cont, body_len))
}

pub fn read_header_from_bytes(
    bytes: Vec<u8>,
    options: &LoadOptions,
) -> Result<(Hierarchy, ReadBodyContinuation, u64)> {
    let (header_len, hierarchy, lookup) =
        read_hierarchy(&mut std::io::Cursor::new(&bytes), options)?;
    let body_len = (bytes.len() - header_len) as u64;
    let cont = ReadBodyContinuation {
        multi_thread: options.multi_thread,
        header_len,
        lookup,
        input: Input::Bytes(bytes),
    };
    Ok((hierarchy, cont, body_len))
}

pub struct ReadBodyContinuation {
    multi_thread: bool,
    header_len: usize,
    lookup: IdLookup,
    input: Input,
}

enum Input {
    Bytes(Vec<u8>),
    File(memmap2::Mmap),
}

pub fn read_body(
    data: ReadBodyContinuation,
    hierarchy: &Hierarchy,
    progress: Option<ProgressCount>,
) -> Result<(SignalSource, TimeTable)> {
    let (source, time_table) = match data.input {
        Input::Bytes(mmap) => read_values(
            &mmap[data.header_len..],
            data.multi_thread,
            hierarchy,
            &data.lookup,
            progress,
        )?,
        Input::File(bytes) => read_values(
            &bytes[data.header_len..],
            data.multi_thread,
            hierarchy,
            &data.lookup,
            progress,
        )?,
    };
    Ok((source, time_table))
}

const FST_SUP_VAR_DATA_TYPE_BITS: u32 = 10;
const FST_SUP_VAR_DATA_TYPE_MASK: u64 = (1 << FST_SUP_VAR_DATA_TYPE_BITS) - 1;

// VCD attributes are a GTKWave extension which is also used by nvc
fn parse_attribute(
    tokens: Vec<&[u8]>,
    path_names: &mut HashMap<u64, HierarchyStringId>,
    h: &mut HierarchyBuilder,
) -> Result<Option<Attribute>> {
    match tokens[1] {
        b"02" => {
            // FstHierarchyEntry::VhdlVarInfo
            if tokens.len() != 4 {
                return Err(unexpected_n_tokens("attribute", &tokens));
            }
            let type_name = std::str::from_utf8(tokens[2])?.to_string();
            let arg = std::str::from_utf8(tokens[3])?.parse::<u64>()?;
            let var_type =
                FstVhdlVarType::try_from_primitive((arg >> FST_SUP_VAR_DATA_TYPE_BITS) as u8)?;
            let data_type =
                FstVhdlDataType::try_from_primitive((arg & FST_SUP_VAR_DATA_TYPE_MASK) as u8)?;
            Ok(Some(Attribute::VhdlTypeInfo(
                type_name, var_type, data_type,
            )))
        }
        b"03" => {
            // FstHierarchyEntry::PathName
            if tokens.len() != 4 {
                return Err(unexpected_n_tokens("attribute", &tokens));
            }
            let path = std::str::from_utf8(tokens[2])?.to_string();
            let id = std::str::from_utf8(tokens[3])?.parse::<u64>()?;
            let string_ref = h.add_string(path);
            path_names.insert(id, string_ref);
            Ok(None)
        }
        b"04" => {
            // FstHierarchyEntry::SourceStem
            if tokens.len() != 4 {
                // TODO: GTKWave might actually generate 5 tokens in order to include whether it is the
                //       instance of the normal source path
                return Err(unexpected_n_tokens("attribute", &tokens));
            }
            let path_id = std::str::from_utf8(tokens[2])?.parse::<u64>()?;
            let line = std::str::from_utf8(tokens[3])?.parse::<u64>()?;
            let is_instance = false;
            Ok(Some(Attribute::SourceLoc(
                path_names[&path_id],
                line,
                is_instance,
            )))
        }
        _ => Err(VcdParseError::VcdUnsupportedAttributeType(
            iter_bytes_to_list_str(tokens.iter()),
        )),
    }
}

type IdLookup = Option<HashMap<Vec<u8>, SignalRef>>;

fn read_hierarchy(
    input: &mut (impl BufRead + Seek),
    options: &LoadOptions,
) -> Result<(usize, Hierarchy, IdLookup)> {
    let start = input.stream_position().unwrap();
    let mut h = HierarchyBuilder::new(FileFormat::Vcd);
    let mut attributes = Vec::new();
    let mut path_names = HashMap::new();
    // this map is used to translate identifiers to signal references for cases where we detect ids that are too large
    let mut id_map: HashMap<Vec<u8>, SignalRef> = HashMap::new();
    let mut use_id_map = false;
    let mut var_count = 0u64;

    let mut id_to_signal_ref = |id: &[u8], var_count: u64| -> SignalRef {
        // currently we only make a decision of whether to switch to a hash_map based lookup when we are at the first variable
        if var_count == 0 {
            if let Some(id_value) = id_to_int(id) {
                if id_value < 1024 * 1024 {
                    return SignalRef::from_index(id_value as usize).unwrap();
                } else {
                    use_id_map = true;
                }
            } else {
                use_id_map = true;
            }
        }

        if use_id_map {
            match id_map.get(id) {
                Some(signal_ref) => *signal_ref,
                None => {
                    let signal_ref = SignalRef::from_index(id_map.len() + 1).unwrap();
                    id_map.insert(id.to_vec(), signal_ref);
                    signal_ref
                }
            }
        } else {
            SignalRef::from_index(id_to_int(id).unwrap() as usize).unwrap()
        }
    };

    let callback = |cmd: HeaderCmd| match cmd {
        HeaderCmd::Scope(tpe, name) => {
            let flatten = options.remove_scopes_with_empty_name && name.is_empty();
            let (declaration_source, instance_source) =
                parse_scope_attributes(&mut attributes, &mut h)?;
            let name = h.add_string(std::str::from_utf8(name)?.to_string());
            h.add_scope(
                name,
                None, // VCDs do not contain component names
                convert_scope_tpe(tpe)?,
                declaration_source,
                instance_source,
                flatten,
            );
            Ok(())
        }
        HeaderCmd::UpScope => {
            h.pop_scope();
            Ok(())
        }
        HeaderCmd::Var(tpe, size, id, name) => {
            let length = match std::str::from_utf8(size).unwrap().parse::<u32>() {
                Ok(len) => len,
                Err(_) => {
                    return Err(VcdParseError::VcdVarLengthParsing(
                        String::from_utf8_lossy(size).to_string(),
                        String::from_utf8_lossy(name).to_string(),
                    ));
                }
            };
            let (var_name, index, scopes) = parse_name(name)?;
            let (type_name, var_type, enum_type) =
                parse_var_attributes(&mut attributes, convert_var_tpe(tpe)?, &var_name)?;
            let name = h.add_string(var_name);
            let type_name = type_name.map(|s| h.add_string(s));
            let num_scopes = scopes.len();
            h.add_array_scopes(scopes);
            h.add_var(
                name,
                var_type,
                VarDirection::vcd_default(),
                length,
                index,
                id_to_signal_ref(id, var_count),
                enum_type,
                type_name,
            );
            h.pop_scopes(num_scopes);
            var_count += 1;
            Ok(())
        }
        HeaderCmd::Date(value) => {
            h.set_date(String::from_utf8_lossy(value).to_string());
            Ok(())
        }
        HeaderCmd::Version(value) => {
            h.set_version(String::from_utf8_lossy(value).to_string());
            Ok(())
        }
        HeaderCmd::Comment(value) => {
            h.add_comment(String::from_utf8_lossy(value).to_string());
            Ok(())
        }
        HeaderCmd::Timescale(factor, unit) => {
            let factor_int = std::str::from_utf8(factor)?.parse::<u32>()?;
            let value = Timescale::new(factor_int, convert_timescale_unit(unit));
            h.set_timescale(value);
            Ok(())
        }
        HeaderCmd::MiscAttribute(tokens) => {
            if let Some(attr) = parse_attribute(tokens, &mut path_names, &mut h)? {
                attributes.push(attr);
            }
            Ok(())
        }
    };

    read_vcd_header(input, callback)?;
    let end = input.stream_position().unwrap();
    let hierarchy = h.finish();
    let lookup = if use_id_map { Some(id_map) } else { None };
    Ok(((end - start) as usize, hierarchy, lookup))
}

/// Splits a full name into:
/// 1. the variable name
/// 2. the bit index
/// 3. any extra scopes generated by a multidimensional arrays
pub fn parse_name(name: &[u8]) -> Result<(String, Option<VarIndex>, Vec<String>)> {
    let last = match name.last() {
        // special case for empty name
        None => return Ok(("".to_string(), None, vec![])),
        Some(l) => *l,
    };
    debug_assert!(
        last != b' ',
        "we assume that the final character is not a space!"
    );
    debug_assert!(
        name[0] != b' ',
        "we assume that the first character is not a space!"
    );
    debug_assert!(
        name[0] != b'[',
        "we assume that the first character is not `[`!"
    );

    // find the bit index from the back
    let (mut name, index) = if last == b']' {
        let index_start = match find_last(name, b'[') {
            Some(s) => s,
            None => {
                return Err(VcdParseError::VcdVarNameParsing(
                    String::from_utf8_lossy(name).to_string(),
                ))
            }
        };
        let inner_index = &name[index_start + 1..(name.len() - 1)];
        let remaining_name = trim_right(&name[..index_start]);
        (remaining_name, parse_inner_index(inner_index))
    } else {
        (name, None)
    };

    // see if there are any other indices from multidimensional arrays
    let mut indices = vec![];
    while name.last().cloned() == Some(b']') {
        let index_start = match find_last(name, b'[') {
            Some(s) => s,
            None => {
                return Err(VcdParseError::VcdVarNameParsing(
                    String::from_utf8_lossy(name).to_string(),
                ))
            }
        };
        let index = &name[index_start..(name.len())];
        indices.push(String::from_utf8_lossy(index).to_string());
        name = trim_right(&name[..index_start]);
    }

    let name = String::from_utf8_lossy(name).to_string();

    if indices.is_empty() {
        Ok((name, index, indices))
    } else {
        // if there are indices, the name actually becomes part of the scope
        let mut scopes = Vec::with_capacity(indices.len());
        scopes.push(name);
        while indices.len() > 1 {
            scopes.push(indices.pop().unwrap());
        }
        let final_name = indices.pop().unwrap();
        Ok((final_name, index, scopes))
    }
}

#[inline]
fn trim_right(mut name: &[u8]) -> &[u8] {
    while name.last().cloned() == Some(b' ') {
        name = &name[..(name.len() - 1)];
    }
    name
}

#[inline]
fn find_last(haystack: &[u8], needle: u8) -> Option<usize> {
    let from_back = haystack.iter().rev().position(|b| *b == needle)?;
    Some(haystack.len() - from_back - 1)
}

#[inline]
fn parse_inner_index(index: &[u8]) -> Option<VarIndex> {
    let sep = index.iter().position(|b| *b == b':');
    match sep {
        None => {
            let inner_str = std::str::from_utf8(index).unwrap();
            let bit = inner_str.parse::<i32>().unwrap();
            Some(VarIndex::new(bit, bit))
        }
        Some(pos) => {
            let msb_bytes = &index[0..pos];
            let msb_str = std::str::from_utf8(msb_bytes).unwrap();
            let msb = msb_str.parse::<i32>().unwrap();
            let lsb_bytes = &index[(pos + 1)..index.len()];
            let lsb_str = std::str::from_utf8(lsb_bytes).unwrap();
            let lsb = lsb_str.parse::<i32>().unwrap();
            Some(VarIndex::new(msb, lsb))
        }
    }
}

fn convert_timescale_unit(name: &[u8]) -> TimescaleUnit {
    match name {
        b"fs" => TimescaleUnit::FemtoSeconds,
        b"ps" => TimescaleUnit::PicoSeconds,
        b"ns" => TimescaleUnit::NanoSeconds,
        b"us" => TimescaleUnit::MicroSeconds,
        b"ms" => TimescaleUnit::MilliSeconds,
        b"s" => TimescaleUnit::Seconds,
        _ => TimescaleUnit::Unknown,
    }
}

fn convert_scope_tpe(tpe: &[u8]) -> Result<ScopeType> {
    match tpe {
        b"module" => Ok(ScopeType::Module),
        b"task" => Ok(ScopeType::Task),
        b"function" => Ok(ScopeType::Function),
        b"begin" => Ok(ScopeType::Begin),
        b"fork" => Ok(ScopeType::Fork),
        b"generate" => Ok(ScopeType::Generate),
        b"struct" => Ok(ScopeType::Struct),
        b"union" => Ok(ScopeType::Union),
        b"class" => Ok(ScopeType::Class),
        b"interface" => Ok(ScopeType::Interface),
        b"package" => Ok(ScopeType::Package),
        b"program" => Ok(ScopeType::Program),
        b"vhdl_architecture" => Ok(ScopeType::VhdlArchitecture),
        b"vhdl_procedure" => Ok(ScopeType::VhdlProcedure),
        b"vhdl_function" => Ok(ScopeType::VhdlFunction),
        b"vhdl_record" => Ok(ScopeType::VhdlRecord),
        b"vhdl_process" => Ok(ScopeType::VhdlProcess),
        b"vhdl_block" => Ok(ScopeType::VhdlBlock),
        b"vhdl_for_generate" => Ok(ScopeType::VhdlForGenerate),
        b"vhdl_if_generate" => Ok(ScopeType::VhdlIfGenerate),
        b"vhdl_generate" => Ok(ScopeType::VhdlGenerate),
        b"vhdl_package" => Ok(ScopeType::VhdlPackage),
        _ => Err(VcdParseError::VcdUnknownScopeType(
            String::from_utf8_lossy(tpe).to_string(),
        )),
    }
}

fn convert_var_tpe(tpe: &[u8]) -> Result<VarType> {
    match tpe {
        b"wire" => Ok(VarType::Wire),
        b"reg" => Ok(VarType::Reg),
        b"parameter" => Ok(VarType::Parameter),
        b"integer" => Ok(VarType::Integer),
        b"string" => Ok(VarType::String),
        b"event" => Ok(VarType::Event),
        b"real" => Ok(VarType::Real),
        b"real_parameter" => Ok(VarType::Parameter),
        b"supply0" => Ok(VarType::Supply0),
        b"supply1" => Ok(VarType::Supply1),
        b"time" => Ok(VarType::Time),
        b"tri" => Ok(VarType::Tri),
        b"triand" => Ok(VarType::TriAnd),
        b"trior" => Ok(VarType::TriOr),
        b"trireg" => Ok(VarType::TriReg),
        b"tri0" => Ok(VarType::Tri0),
        b"tri1" => Ok(VarType::Tri1),
        b"wand" => Ok(VarType::WAnd),
        b"wor" => Ok(VarType::WOr),
        b"logic" => Ok(VarType::Logic),
        b"port" => Ok(VarType::Port),
        b"sparray" => Ok(VarType::SparseArray),
        b"realtime" => Ok(VarType::RealTime),
        b"bit" => Ok(VarType::Bit),
        b"int" => Ok(VarType::Int),
        b"shortint" => Ok(VarType::ShortInt),
        b"longint" => Ok(VarType::LongInt),
        b"byte" => Ok(VarType::Byte),
        b"enum" => Ok(VarType::Enum),
        b"shortread" => Ok(VarType::ShortReal),
        _ => Err(VcdParseError::VcdUnknownVarType(
            String::from_utf8_lossy(tpe).to_string(),
        )),
    }
}

const ID_CHAR_MIN: u8 = b'!';
const ID_CHAR_MAX: u8 = b'~';
const NUM_ID_CHARS: u64 = (ID_CHAR_MAX - ID_CHAR_MIN + 1) as u64;

/// Copied from https://github.com/kevinmehall/rust-vcd, licensed under MIT
#[inline]
fn id_to_int(id: &[u8]) -> Option<u64> {
    if id.is_empty() {
        return None;
    }
    let mut result = 0u64;
    for &i in id.iter().rev() {
        if !(ID_CHAR_MIN..=ID_CHAR_MAX).contains(&i) {
            return None;
        }
        let c = ((i - ID_CHAR_MIN) as u64) + 1;
        result = match result
            .checked_mul(NUM_ID_CHARS)
            .and_then(|x| x.checked_add(c))
        {
            None => return None,
            Some(value) => value,
        };
    }
    Some(result - 1)
}

#[inline]
fn unexpected_n_tokens(cmd: &str, tokens: &[&[u8]]) -> VcdParseError {
    VcdParseError::VcdUnexpectedNumberOfTokens(
        cmd.to_string(),
        iter_bytes_to_list_str(tokens.iter()),
    )
}

fn read_vcd_header(
    input: &mut impl BufRead,
    mut callback: impl FnMut(HeaderCmd) -> Result<()>,
) -> Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(128);
    loop {
        buf.clear();
        let (cmd, body) = read_command(input, &mut buf)?;
        let parsed = match cmd {
            VcdCmd::Scope => {
                let tokens = find_tokens(body);
                let name = tokens.get(1).cloned().unwrap_or(&[] as &[u8]);
                HeaderCmd::Scope(tokens[0], name)
            }
            VcdCmd::Var => {
                let tokens = find_tokens(body);
                // the actual variable name could be represented by a variable number of tokens,
                // thus we combine all trailing tokens together
                if tokens.len() < 4 {
                    return Err(unexpected_n_tokens("variable", &tokens));
                }
                // concatenate all trailing tokens
                let body_start = body.as_ptr() as u64;
                let name_start = tokens[3].as_ptr() as u64 - body_start;
                let last_token = tokens.last().unwrap();
                let name_end = last_token.as_ptr() as u64 - body_start + last_token.len() as u64;
                let name = &body[name_start as usize..name_end as usize];
                HeaderCmd::Var(tokens[0], tokens[1], tokens[2], name)
            }
            VcdCmd::UpScope => HeaderCmd::UpScope,
            VcdCmd::Date => HeaderCmd::Date(body),
            VcdCmd::Comment => HeaderCmd::Comment(body),
            VcdCmd::Version => HeaderCmd::Version(body),
            VcdCmd::Timescale => {
                let tokens = find_tokens(body);
                let (factor, unit) = match tokens.len() {
                    1 => {
                        // find the first non-numeric character
                        let token = tokens[0];
                        match token.iter().position(|c| *c < b'0' || *c > b'9') {
                            None => (token, &[] as &[u8]),
                            Some(pos) => (&token[..pos], &token[pos..]),
                        }
                    }
                    2 => (tokens[0], tokens[1]),
                    _ => {
                        return Err(VcdParseError::VcdUnexpectedNumberOfTokens(
                            "timescale".to_string(),
                            iter_bytes_to_list_str(tokens.iter()),
                        ))
                    }
                };
                HeaderCmd::Timescale(factor, unit)
            }
            VcdCmd::EndDefinitions => {
                // header is done
                return Ok(());
            }
            VcdCmd::Attribute => {
                let tokens = find_tokens(body);
                if tokens.len() < 3 {
                    return Err(VcdParseError::VcdUnexpectedNumberOfTokens(
                        "attribute".to_string(),
                        iter_bytes_to_list_str(tokens.iter()),
                    ));
                }
                match tokens[0] {
                    b"misc" => HeaderCmd::MiscAttribute(tokens),
                    _ => {
                        return Err(VcdParseError::VcdUnsupportedAttributeType(
                            iter_bytes_to_list_str(tokens.iter()),
                        ))
                    }
                }
            }
        };
        (callback)(parsed)?;
    }
}

const VCD_DATE: &[u8] = b"date";
const VCD_TIMESCALE: &[u8] = b"timescale";
const VCD_VAR: &[u8] = b"var";
const VCD_SCOPE: &[u8] = b"scope";
const VCD_UP_SCOPE: &[u8] = b"upscope";
const VCD_COMMENT: &[u8] = b"comment";
const VCD_VERSION: &[u8] = b"version";
const VCD_END_DEFINITIONS: &[u8] = b"enddefinitions";
/// This might be an unofficial extension used by VHDL simulators.
const VCD_ATTRIBUTE_BEGIN: &[u8] = b"attrbegin";
const VCD_COMMANDS: [&[u8]; 9] = [
    VCD_DATE,
    VCD_TIMESCALE,
    VCD_VAR,
    VCD_SCOPE,
    VCD_UP_SCOPE,
    VCD_COMMENT,
    VCD_VERSION,
    VCD_END_DEFINITIONS,
    VCD_ATTRIBUTE_BEGIN,
];

/// Used to show all commands when printing an error message.
fn get_vcd_command_str() -> String {
    iter_bytes_to_list_str(VCD_COMMANDS.iter())
}

fn iter_bytes_to_list_str<'a, I>(bytes: I) -> String
where
    I: Iterator<Item = &'a &'a [u8]>,
{
    bytes
        .map(|c| String::from_utf8_lossy(c))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, PartialEq)]
enum VcdCmd {
    Date,
    Timescale,
    Var,
    Scope,
    UpScope,
    Comment,
    Version,
    EndDefinitions,
    Attribute,
}

impl VcdCmd {
    fn from_bytes(name: &[u8]) -> Option<Self> {
        match name {
            VCD_VAR => Some(VcdCmd::Var),
            VCD_SCOPE => Some(VcdCmd::Scope),
            VCD_UP_SCOPE => Some(VcdCmd::UpScope),
            VCD_DATE => Some(VcdCmd::Date),
            VCD_TIMESCALE => Some(VcdCmd::Timescale),
            VCD_COMMENT => Some(VcdCmd::Comment),
            VCD_VERSION => Some(VcdCmd::Version),
            VCD_END_DEFINITIONS => Some(VcdCmd::EndDefinitions),
            VCD_ATTRIBUTE_BEGIN => Some(VcdCmd::Attribute),
            _ => None,
        }
    }

    fn from_bytes_or_panic(name: &[u8]) -> Self {
        match Self::from_bytes(name) {
            None => {
                panic!(
                    "Unexpected VCD command {}. Supported commands are: {:?}",
                    String::from_utf8_lossy(name),
                    get_vcd_command_str()
                );
            }
            Some(cmd) => cmd,
        }
    }
}

/// Tries to guess whether this input could be a VCD by looking at the first token.
pub fn is_vcd(input: &mut (impl BufRead + Seek)) -> bool {
    let is_vcd = matches!(internal_is_vcd(input), Ok(true));
    // try to reset input
    let _ = input.seek(std::io::SeekFrom::Start(0));
    is_vcd
}

/// Returns an error or false if not a vcd. Returns Ok(true) only if we think it is a vcd.
fn internal_is_vcd(input: &mut (impl BufRead + Seek)) -> Result<bool> {
    let mut buf = Vec::with_capacity(64);
    let (_cmd, _body) = read_command(input, &mut buf)?;
    Ok(true)
}

/// Reads in a command until the `$end`. Uses buf to store the read data.
/// Returns the name and the body of the command.
fn read_command<'a>(input: &mut impl BufRead, buf: &'a mut Vec<u8>) -> Result<(VcdCmd, &'a [u8])> {
    // start out with an empty buffer
    assert!(buf.is_empty());

    // skip over any preceding whitespace
    let start_char = skip_whitespace(input)?;

    if start_char != b'$' {
        return Err(VcdParseError::VcdStartChar(
            String::from_utf8_lossy(&[start_char]).to_string(),
        ));
    }

    // read the rest of the command into the buffer
    read_token(input, buf)?;

    // check to see if this is a valid command
    let cmd = VcdCmd::from_bytes_or_panic(buf);
    buf.clear();

    // read until we find the end token
    read_until_end_token(input, buf)?;

    // return the name and body of the command
    Ok((cmd, &buf[..]))
}

#[inline]
fn find_tokens(line: &[u8]) -> Vec<&[u8]> {
    line.split(|c| matches!(*c, b' '))
        .filter(|e| !e.is_empty())
        .collect()
}

#[inline]
fn read_until_end_token(input: &mut impl BufRead, buf: &mut Vec<u8>) -> std::io::Result<()> {
    // count how many characters of the $end token we have recognized
    let mut end_index = 0;
    // we skip any whitespace at the beginning, but not between tokens
    let mut skipping_preceding_whitespace = true;
    loop {
        let byte = read_byte(input)?;
        if skipping_preceding_whitespace {
            match byte {
                b' ' | b'\n' | b'\r' | b'\t' => {
                    continue;
                }
                _ => {
                    skipping_preceding_whitespace = false;
                }
            }
        }
        // we always append and then later drop the `$end` bytes.
        buf.push(byte);
        end_index = match (end_index, byte) {
            (0, b'$') => 1,
            (1, b'e') => 2,
            (2, b'n') => 3,
            (3, b'd') => {
                // we are done!
                buf.truncate(buf.len() - 4); // drop $end
                right_strip(buf);
                return Ok(());
            }
            _ => 0, // reset
        };
    }
}

#[inline]
fn read_token(input: &mut impl BufRead, buf: &mut Vec<u8>) -> std::io::Result<()> {
    loop {
        let byte = read_byte(input)?;
        match byte {
            b' ' | b'\n' | b'\r' | b'\t' => {
                return Ok(());
            }
            other => {
                buf.push(other);
            }
        }
    }
}

/// Advances the input until the first non-whitespace character which is then returned.
#[inline]
fn skip_whitespace(input: &mut impl BufRead) -> std::io::Result<u8> {
    loop {
        let byte = read_byte(input)?;
        match byte {
            b' ' | b'\n' | b'\r' | b'\t' => {}
            other => return Ok(other),
        }
    }
}

#[inline]
fn read_byte(input: &mut impl BufRead) -> std::io::Result<u8> {
    let mut buf = [0u8; 1];
    input.read_exact(&mut buf)?;
    Ok(buf[0])
}

#[inline]
fn right_strip(buf: &mut Vec<u8>) {
    while !buf.is_empty() {
        match buf.last().unwrap() {
            b' ' | b'\n' | b'\r' | b'\t' => buf.pop(),
            _ => break,
        };
    }
}

enum HeaderCmd<'a> {
    Date(&'a [u8]),
    Version(&'a [u8]),
    Comment(&'a [u8]),
    Timescale(&'a [u8], &'a [u8]), // factor, unit
    Scope(&'a [u8], &'a [u8]),     // tpe, name
    UpScope,
    Var(&'a [u8], &'a [u8], &'a [u8], &'a [u8]), // tpe, size, id, name
    /// Misc attributes are emitted by nvc (VHDL sim) and fst2vcd (included with GTKwave).
    MiscAttribute(Vec<&'a [u8]>),
}

/// The minimum number of bytes we want to read per thread.
const MIN_CHUNK_SIZE: usize = 8 * 1024;

#[inline]
pub fn usize_div_ceil(a: usize, b: usize) -> usize {
    (a + b - 1) / b
}

#[inline]
pub fn u32_div_ceil(a: u32, b: u32) -> u32 {
    (a + b - 1) / b
}

/// Returns starting byte and read length for every thread. Note that read-length is just an
/// approximation and the thread might have to read beyond or might also run out of data before
/// reaching read length.
#[inline]
fn determine_thread_chunks(body_len: usize) -> Vec<(usize, usize)> {
    let max_threads = rayon::current_num_threads();
    let number_of_threads_for_min_chunk_size = usize_div_ceil(body_len, MIN_CHUNK_SIZE);
    let num_threads = std::cmp::min(max_threads, number_of_threads_for_min_chunk_size);
    let chunk_size = usize_div_ceil(body_len, num_threads);
    // TODO: for large file it might make sense to have more chunks than threads
    (0..num_threads)
        .map(|ii| (ii * chunk_size, chunk_size))
        .collect()
}

/// Reads the body of a VCD with multiple threads
fn read_values(
    input: &[u8],
    multi_thread: bool,
    hierarchy: &Hierarchy,
    lookup: &IdLookup,
    progress: Option<ProgressCount>,
) -> Result<(SignalSource, TimeTable)> {
    if multi_thread {
        let chunks = determine_thread_chunks(input.len());
        let encoders: Vec<crate::wavemem::Encoder> = chunks
            .par_iter()
            .map(|(start, len)| {
                let is_first = *start == 0;
                // check to see if the chunk start on a new line
                let starts_on_new_line = if is_first {
                    true
                } else {
                    let before = input[*start - 1];
                    // TODO: deal with \n\r
                    before == b'\n'
                };
                read_single_stream_of_values(
                    &input[*start..],
                    *len - 1,
                    is_first,
                    starts_on_new_line,
                    hierarchy,
                    lookup,
                    progress.clone(),
                )
            })
            .collect();

        // combine encoders
        let mut encoder_iter = encoders.into_iter();
        let mut encoder = encoder_iter.next().unwrap();
        for other in encoder_iter {
            encoder.append(other);
        }
        Ok(encoder.finish())
    } else {
        let encoder = read_single_stream_of_values(
            input,
            input.len() - 1,
            true,
            true,
            hierarchy,
            lookup,
            progress,
        );
        Ok(encoder.finish())
    }
}

fn read_single_stream_of_values(
    input: &[u8],
    stop_pos: usize,
    is_first: bool,
    starts_on_new_line: bool,
    hierarchy: &Hierarchy,
    lookup: &IdLookup,
    progress: Option<ProgressCount>,
) -> crate::wavemem::Encoder {
    let mut encoder = crate::wavemem::Encoder::new(hierarchy);

    let (input2, offset) = if starts_on_new_line {
        (input, 0)
    } else {
        advance_to_first_newline(input)
    };
    let mut reader = BodyReader::new(input2);
    // We only start recording once we have encountered our first time step
    let mut found_first_time_step = false;

    // progress tracking
    let mut last_reported_pos = 0;
    let report_increments = std::cmp::max(input2.len() as u64 / 1000, 512);

    loop {
        if let Some((pos, cmd)) = reader.next() {
            if (pos + offset) > stop_pos {
                if let BodyCmd::Time(_to) = cmd {
                    if let Some(p) = progress.as_ref() {
                        let increment = (pos - last_reported_pos) as u64;
                        p.fetch_add(increment, Ordering::SeqCst);
                    }
                    break; // stop before the next time value when we go beyond the stop position
                }
            }
            if let Some(p) = progress.as_ref() {
                let increment = (pos - last_reported_pos) as u64;
                if increment >= report_increments {
                    last_reported_pos = pos;
                    p.fetch_add(increment, Ordering::SeqCst);
                }
            }
            match cmd {
                BodyCmd::Time(value) => {
                    found_first_time_step = true;
                    let int_value = std::str::from_utf8(value).unwrap().parse::<u64>().unwrap();
                    encoder.time_change(int_value);
                }
                BodyCmd::Value(value, id) => {
                    // In the first thread, we might encounter a dump values which dumps all initial values
                    // without specifying a timestamp
                    if is_first && !found_first_time_step {
                        encoder.time_change(0);
                        found_first_time_step = true;
                    }
                    if found_first_time_step {
                        let num_id = match lookup {
                            None => id_to_int(id).unwrap(),
                            Some(lookup) => lookup[id].index() as u64,
                        };
                        encoder.vcd_value_change(num_id, value);
                    }
                }
            };
        } else {
            if let Some(p) = progress.as_ref() {
                let increment = (reader.pos - last_reported_pos) as u64;
                p.fetch_add(increment, Ordering::SeqCst);
            }
            break; // done, no more values to read
        }
    }

    encoder
}

#[inline]
fn advance_to_first_newline(input: &[u8]) -> (&[u8], usize) {
    for (pos, byte) in input.iter().enumerate() {
        if *byte == b'\n' {
            return (&input[pos..], pos);
        }
    }
    (&[], 0) // no whitespaces found
}

struct BodyReader<'a> {
    input: &'a [u8],
    // state
    pos: usize,
    // statistics
    lines_read: usize,
}

const ASCII_ZERO: &[u8] = b"0";

impl<'a> BodyReader<'a> {
    fn new(input: &'a [u8]) -> Self {
        BodyReader {
            input,
            pos: 0,
            lines_read: 0,
        }
    }

    #[inline]
    fn try_finish_token(
        &mut self,
        pos: usize,
        token_start: &mut Option<usize>,
        prev_token: &mut Option<&'a [u8]>,
        search_for_end: &mut bool,
    ) -> Option<BodyCmd<'a>> {
        match *token_start {
            None => None,
            Some(start) => {
                let token = &self.input[start..pos];
                if token.is_empty() {
                    return None;
                }
                if *search_for_end {
                    *search_for_end = token != b"$end";
                    // consume token and return
                    *token_start = None;
                    return None;
                }
                let ret = match *prev_token {
                    None => {
                        if token.len() == 1 {
                            // too short
                            return None;
                        }
                        // 1-token commands are binary changes or time commands
                        match token[0] {
                            b'#' => Some(BodyCmd::Time(&token[1..])),
                            b'0' | b'1' | b'z' | b'Z' | b'x' | b'X' | b'h' | b'H' | b'u' | b'U'
                            | b'w' | b'W' | b'l' | b'L' | b'-' => {
                                Some(BodyCmd::Value(&token[0..1], &token[1..]))
                            }
                            _ => {
                                if token == b"$dumpall" {
                                    // interpret dumpall as indicating timestep zero
                                    return Some(BodyCmd::Time(ASCII_ZERO));
                                }
                                if token == b"$comment" {
                                    // drop token, but start searching for $end in order to skip the comment
                                    *search_for_end = true;
                                } else if token != b"$dumpvars"
                                    && token != b"$end"
                                    && token != b"$dumpoff"
                                {
                                    // ignore dumpvars, dumpoff, and end command
                                    *prev_token = Some(token);
                                }
                                None
                            }
                        }
                    }
                    Some(first) => {
                        let cmd = match first[0] {
                            b'b' | b'B' | b'r' | b'R' | b's' | b'S' => {
                                BodyCmd::Value(&first[0..], token)
                            }
                            _ => {
                                panic!(
                                    "Unexpected tokens: `{}` and `{}` ({} lines after header)",
                                    String::from_utf8_lossy(first),
                                    String::from_utf8_lossy(token),
                                    self.lines_read
                                );
                            }
                        };
                        *prev_token = None;
                        Some(cmd)
                    }
                };
                *token_start = None;
                ret
            }
        }
    }
}

impl<'a> Iterator for BodyReader<'a> {
    type Item = (usize, BodyCmd<'a>);

    /// returns the starting position and the body of the command
    #[inline]
    fn next(&mut self) -> Option<(usize, BodyCmd<'a>)> {
        if self.pos >= self.input.len() {
            return None; // done!
        }
        let mut token_start: Option<usize> = None;
        let mut prev_token: Option<&'a [u8]> = None;
        let mut pending_lines = 0;
        let mut start_pos = 0;
        // if we encounter a $comment, we will just be searching for a $end token
        let mut search_for_end = false;
        for (offset, b) in self.input[self.pos..].iter().enumerate() {
            let pos = self.pos + offset;
            match b {
                b' ' | b'\n' | b'\r' | b'\t' => {
                    if token_start.is_none() {
                        if *b == b'\n' {
                            self.lines_read += 1;
                        }
                    } else {
                        match self.try_finish_token(
                            pos,
                            &mut token_start,
                            &mut prev_token,
                            &mut search_for_end,
                        ) {
                            None => {
                                if *b == b'\n' {
                                    pending_lines += 1;
                                }
                            }
                            Some(cmd) => {
                                // save state
                                self.pos = pos;
                                self.lines_read += pending_lines;
                                if *b == b'\n' {
                                    self.lines_read += 1;
                                }
                                return Some((start_pos, cmd));
                            }
                        }
                    }
                }
                _ => match token_start {
                    None => {
                        token_start = Some(pos);
                        if prev_token.is_none() {
                            // remember the start of the first token
                            start_pos = pos;
                        }
                    }
                    Some(_) => {}
                },
            }
        }
        // update final position
        self.pos = self.input.len();
        // check to see if there is a final token at the end
        match self.try_finish_token(
            self.pos,
            &mut token_start,
            &mut prev_token,
            &mut search_for_end,
        ) {
            None => {}
            Some(cmd) => {
                return Some((start_pos, cmd));
            }
        }
        // now we are done
        None
    }
}

enum BodyCmd<'a> {
    Time(&'a [u8]),
    Value(&'a [u8], &'a [u8]),
}

impl<'a> Debug for BodyCmd<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyCmd::Time(value) => {
                write!(f, "Time({})", String::from_utf8_lossy(value))
            }
            BodyCmd::Value(value, id) => {
                write!(
                    f,
                    "Value({}, {})",
                    String::from_utf8_lossy(id),
                    String::from_utf8_lossy(value)
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_body_to_vec(input: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let reader = BodyReader::new(input);
        for (_, cmd) in reader {
            let desc = match cmd {
                BodyCmd::Time(value) => {
                    format!("Time({})", std::str::from_utf8(value).unwrap())
                }
                BodyCmd::Value(value, id) => {
                    format!(
                        "{} = {}",
                        std::str::from_utf8(id).unwrap(),
                        std::str::from_utf8(value).unwrap()
                    )
                }
            };
            out.push(desc);
        }
        out
    }

    #[test]
    fn test_read_body() {
        let input = r#"
1I,!
1J,!
1#2!
#2678437829
b00 D2!
b0000 d2!
b11 e2!
b00000 f2!
b10100 g2!
b00000 h2!
b00000 i2!
x(i"
x'i"
x&i"
x%i"
0j2!"#;
        let expected = vec![
            "I,! = 1",
            "J,! = 1",
            "#2! = 1",
            "Time(2678437829)",
            "D2! = b00",
            "d2! = b0000",
            "e2! = b11",
            "f2! = b00000",
            "g2! = b10100",
            "h2! = b00000",
            "i2! = b00000",
            "(i\" = x",
            "'i\" = x",
            "&i\" = x",
            "%i\" = x",
            "j2! = 0",
        ];
        let res = read_body_to_vec(input.as_bytes());
        assert_eq!(res, expected);
    }

    #[test]
    fn test_read_command() {
        let mut buf = Vec::with_capacity(128);
        let input_0 = b"$upscope $end";
        let (cmd_0, body_0) = read_command(&mut input_0.as_slice(), &mut buf).unwrap();
        assert_eq!(cmd_0, VcdCmd::UpScope);
        assert!(body_0.is_empty());

        // test with more whitespace
        buf.clear();
        let input_1 = b" \t $upscope \n $end  \n ";
        let (cmd_1, body_1) = read_command(&mut input_1.as_slice(), &mut buf).unwrap();
        assert_eq!(cmd_1, VcdCmd::UpScope);
        assert!(body_1.is_empty());
    }

    #[test]
    fn test_id_to_int() {
        assert_eq!(id_to_int(b""), None);
        assert_eq!(id_to_int(b"!"), Some(0));
        assert_eq!(id_to_int(b"#"), Some(2));
        assert_eq!(id_to_int(b"*"), Some(9));
        assert_eq!(id_to_int(b"c"), Some(66));
        assert_eq!(id_to_int(b"#%"), Some(472));
        assert_eq!(id_to_int(b"("), Some(7));
        assert_eq!(id_to_int(b")"), Some(8));
    }

    #[test]
    fn test_find_last() {
        assert_eq!(find_last(b"1234", b'1'), Some(0));
        assert_eq!(find_last(b"1234", b'5'), None);
        assert_eq!(find_last(b"12341", b'1'), Some(4));
    }

    fn do_test_parse_name(full_name: &str, name: &str, index: Option<(i32, i32)>, scopes: &[&str]) {
        let (a_name, a_index, a_scopes) = parse_name(full_name.as_bytes()).unwrap();
        assert_eq!(a_name, name);
        match index {
            None => assert!(a_index.is_none()),
            Some((msb, lsb)) => {
                assert_eq!(a_index.unwrap().msb(), msb);
                assert_eq!(a_index.unwrap().lsb(), lsb);
            }
        }
        assert_eq!(a_scopes, scopes);
    }

    #[test]
    fn test_parse_name() {
        do_test_parse_name("test", "test", None, &[]);
        do_test_parse_name("test[0]", "test", Some((0, 0)), &[]);
        do_test_parse_name("test [0]", "test", Some((0, 0)), &[]);
        do_test_parse_name("test[1:0]", "test", Some((1, 0)), &[]);
        do_test_parse_name("test [1:0]", "test", Some((1, 0)), &[]);
        do_test_parse_name("test[1:-1]", "test", Some((1, -1)), &[]);
        do_test_parse_name("test [1:-1]", "test", Some((1, -1)), &[]);
        do_test_parse_name("test[0][0]", "[0]", Some((0, 0)), &["test"]);
        do_test_parse_name("test[0] [0]", "[0]", Some((0, 0)), &["test"]);
        do_test_parse_name("test [0] [0]", "[0]", Some((0, 0)), &["test"]);
        do_test_parse_name("test[3][2][0]", "[2]", Some((0, 0)), &["test", "[3]"]);
        do_test_parse_name(
            "test[0][3][2][0]",
            "[2]",
            Some((0, 0)),
            &["test", "[0]", "[3]"],
        );
    }
}
