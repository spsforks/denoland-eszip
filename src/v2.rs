// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::hash::Hash;
use std::mem::size_of;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Poll;
use std::task::Waker;

use deno_ast::EmitOptions;
use deno_ast::EmittedSource;
use deno_ast::SourceMapOption;
use deno_ast::TranspileOptions;
use deno_graph::CapturingModuleParser;
use deno_graph::ModuleGraph;
use deno_graph::ModuleParser;
use deno_graph::ParseOptions;
use deno_npm::resolution::SerializedNpmResolutionSnapshot;
use deno_npm::resolution::SerializedNpmResolutionSnapshotPackage;
use deno_npm::resolution::ValidSerializedNpmResolutionSnapshot;
use deno_npm::NpmPackageId;
use deno_semver::package::PackageReq;
use futures::future::poll_fn;
use futures::io::AsyncReadExt;
use hashlink::linked_hash_map::LinkedHashMap;
use sha2::Digest;
use sha2::Sha256;
pub use url::Url;

use crate::error::ParseError;
use crate::Module;
use crate::ModuleInner;
pub use crate::ModuleKind;

const ESZIP_V2_MAGIC: &[u8; 8] = b"ESZIP_V2";
const ESZIP_V2_1_MAGIC: &[u8; 8] = b"ESZIP2.1";
const ESZIP_V2_2_MAGIC: &[u8; 8] = b"ESZIP2.2";

#[derive(Debug, PartialEq)]
#[repr(u8)]
enum HeaderFrameKind {
  Module = 0,
  Redirect = 1,
  NpmSpecifier = 2,
}

#[derive(Debug, Default, Clone)]
pub struct EszipV2Modules(Arc<Mutex<LinkedHashMap<String, EszipV2Module>>>);

impl EszipV2Modules {
  pub(crate) async fn get_module_source<'a>(
    &'a self,
    specifier: &str,
  ) -> Option<Arc<[u8]>> {
    poll_fn(|cx| {
      let mut modules = self.0.lock().unwrap();
      let module = modules.get_mut(specifier).unwrap();
      let slot = match module {
        EszipV2Module::Module { source, .. } => source,
        EszipV2Module::Redirect { .. } => {
          panic!("redirects are already resolved")
        }
      };
      match slot {
        EszipV2SourceSlot::Pending { wakers, .. } => {
          wakers.push(cx.waker().clone());
          Poll::Pending
        }
        EszipV2SourceSlot::Ready(bytes) => Poll::Ready(Some(bytes.clone())),
        EszipV2SourceSlot::Taken => Poll::Ready(None),
      }
    })
    .await
  }

  pub(crate) async fn take_module_source<'a>(
    &'a self,
    specifier: &str,
  ) -> Option<Arc<[u8]>> {
    poll_fn(|cx| {
      let mut modules = self.0.lock().unwrap();
      let module = modules.get_mut(specifier).unwrap();
      let slot = match module {
        EszipV2Module::Module { ref mut source, .. } => source,
        EszipV2Module::Redirect { .. } => {
          panic!("redirects are already resolved")
        }
      };
      match slot {
        EszipV2SourceSlot::Pending { wakers, .. } => {
          wakers.push(cx.waker().clone());
          return Poll::Pending;
        }
        EszipV2SourceSlot::Ready(_) => {}
        EszipV2SourceSlot::Taken => return Poll::Ready(None),
      };
      let EszipV2SourceSlot::Ready(bytes) =
        std::mem::replace(slot, EszipV2SourceSlot::Taken)
      else {
        unreachable!()
      };
      Poll::Ready(Some(bytes))
    })
    .await
  }

  pub(crate) async fn get_module_source_map<'a>(
    &'a self,
    specifier: &str,
  ) -> Option<Arc<[u8]>> {
    poll_fn(|cx| {
      let mut modules = self.0.lock().unwrap();
      let module = modules.get_mut(specifier).unwrap();
      let slot = match module {
        EszipV2Module::Module { source_map, .. } => source_map,
        EszipV2Module::Redirect { .. } => {
          panic!("redirects are already resolved")
        }
      };
      match slot {
        EszipV2SourceSlot::Pending { wakers, .. } => {
          wakers.push(cx.waker().clone());
          Poll::Pending
        }
        EszipV2SourceSlot::Ready(bytes) => Poll::Ready(Some(bytes.clone())),
        EszipV2SourceSlot::Taken => Poll::Ready(None),
      }
    })
    .await
  }

  pub(crate) async fn take_module_source_map<'a>(
    &'a self,
    specifier: &str,
  ) -> Option<Arc<[u8]>> {
    let source = poll_fn(|cx| {
      let mut modules = self.0.lock().unwrap();
      let module = modules.get_mut(specifier).unwrap();
      let slot = match module {
        EszipV2Module::Module { source_map, .. } => source_map,
        EszipV2Module::Redirect { .. } => {
          panic!("redirects are already resolved")
        }
      };
      match slot {
        EszipV2SourceSlot::Pending { wakers, .. } => {
          wakers.push(cx.waker().clone());
          Poll::Pending
        }
        EszipV2SourceSlot::Ready(bytes) => Poll::Ready(Some(bytes.clone())),
        EszipV2SourceSlot::Taken => Poll::Ready(None),
      }
    })
    .await;

    // Drop the source map from memory.
    let mut modules = self.0.lock().unwrap();
    let module = modules.get_mut(specifier).unwrap();
    match module {
      EszipV2Module::Module { source_map, .. } => {
        *source_map = EszipV2SourceSlot::Taken;
      }
      EszipV2Module::Redirect { .. } => {
        panic!("redirects are already resolved")
      }
    };
    source
  }
}

#[derive(Debug, Clone, Copy)]
struct Options {
  /// Hash Function used to checksum the module's source code.
  ///
  /// If the eszip does not include the option, it defaults to SHA256. If the parser does not know the hash function,
  /// degrades to not checksuming
  source_hash_function: Option<HashFunction>,

  /// Size in Bytes of the hash function digest.
  ///
  /// Defaults to the known length of the configured hash function. Useful in order to ensure forwards compatibility,
  /// otherwise the parser does not know how many bytes to read.
  source_digest_size: Option<u8>,
}

impl Default for Options {
  fn default() -> Self {
    Self {
      source_hash_function: Some(HashFunction::default()),
      source_digest_size: Default::default(),
    }
  }
}

impl Options {
  /// Get the size in Bytes of the source hashes
  ///
  /// If the eszip has an explicit digest size, returns that. Otherwise, returns
  /// the default digest size of the [`Self::source_hash_function`]. If the eszip
  /// does not have either, returns `None`.
  fn source_digest_size(self) -> Option<u8> {
    self
      .source_digest_size
      .or_else(|| Some(self.source_hash_function?.digest_size()))
  }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HashFunction {
  #[default]
  Sha256 = 0,
  Crc32 = 1,
  XxHash = 2,
}

impl HashFunction {
  const fn digest_size(self) -> u8 {
    match self {
      Self::Sha256 => 32,
      Self::Crc32 => 4,
      Self::XxHash => 8,
    }
  }

  fn from_u8(discriminant: u8) -> Option<Self> {
    Some(match discriminant {
      0 => Self::Sha256,
      1 => Self::Crc32,
      2 => Self::XxHash,
      _ => return None,
    })
  }

  fn hash(self, bytes: &[u8]) -> Vec<u8> {
    match self {
      Self::Sha256 => Sha256::digest(bytes).as_slice().to_vec(),
      Self::Crc32 => crc32fast::hash(bytes).to_be_bytes().into(),
      Self::XxHash => todo!(),
    }
  }
}

/// Version 2 of the Eszip format. This format supports streaming sources and
/// source maps.
#[derive(Debug, Default)]
pub struct EszipV2 {
  modules: EszipV2Modules,
  npm_snapshot: Option<ValidSerializedNpmResolutionSnapshot>,
  options: Options,
}

#[derive(Debug)]
pub enum EszipV2Module {
  Module {
    kind: ModuleKind,
    source: EszipV2SourceSlot,
    source_map: EszipV2SourceSlot,
  },
  Redirect {
    target: String,
  },
}

#[derive(Debug)]
pub enum EszipV2SourceSlot {
  Pending {
    offset: usize,
    length: usize,
    wakers: Vec<Waker>,
  },
  Ready(Arc<[u8]>),
  Taken,
}

impl EszipV2SourceSlot {
  fn bytes(&self) -> &[u8] {
    match self {
      EszipV2SourceSlot::Ready(v) => v,
      _ => panic!("EszipV2SourceSlot::bytes() called on a pending slot"),
    }
  }
}

impl EszipV2 {
  pub fn has_magic(buffer: &[u8]) -> bool {
    buffer.len() >= 8
      && (buffer[..8] == *ESZIP_V2_MAGIC
        || buffer[..8] == *ESZIP_V2_1_MAGIC
        || buffer[..8] == *ESZIP_V2_2_MAGIC)
  }

  /// Parse a EszipV2 from an AsyncRead stream. This function returns once the
  /// header section of the eszip has been parsed. Once this function returns,
  /// the data section will not necessarially have been parsed yet. To parse
  /// the data section, poll/await the future returned in the second tuple slot.
  pub async fn parse<R: futures::io::AsyncRead + Unpin>(
    mut reader: futures::io::BufReader<R>,
  ) -> Result<
    (
      EszipV2,
      impl Future<Output = Result<futures::io::BufReader<R>, ParseError>>,
    ),
    ParseError,
  > {
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic).await?;

    if !EszipV2::has_magic(&magic) {
      return Err(ParseError::InvalidV2);
    }

    let supports_npm = magic != *ESZIP_V2_MAGIC;
    let supports_options = magic == *ESZIP_V2_2_MAGIC;

    let mut options = Options::default();

    if supports_options {
      let options_header = HashedSection::read(&mut reader).await?;
      if !options_header.hash_valid() {
        return Err(ParseError::InvalidV22OptionsHeaderHash);
      }
      if options_header.len() % 2 != 0 {
        return Err(ParseError::InvalidV22OptionsHeader);
      }

      for option in options_header.bytes().chunks(2) {
        let (option, value) = (option[0], option[1]);
        match option {
          0 => {
            options.source_hash_function = HashFunction::from_u8(value);
          }
          1 => {
            options.source_digest_size = Some(value);
          }
          _ => {} // Ignore unknown options for forward compatibility
        }
      }
    }

    let modules_header = HashedSection::read(&mut reader).await?;
    if !modules_header.hash_valid() {
      return Err(ParseError::InvalidV2HeaderHash);
    }

    let mut modules = LinkedHashMap::<String, EszipV2Module>::new();
    let mut npm_specifiers = HashMap::new();

    let mut read = 0;

    // This macro reads n number of bytes from the header section. If the header
    // section is not long enough, this function will be early exited with an
    // error.
    macro_rules! read {
      ($n:expr, $err:expr) => {{
        if read + $n > modules_header.len() {
          return Err(ParseError::InvalidV2Header($err));
        }
        let start = read;
        read += $n;
        &modules_header.bytes()[start..read]
      }};
    }

    while read < modules_header.len() {
      let specifier_len =
        u32::from_be_bytes(read!(4, "specifier len").try_into().unwrap())
          as usize;
      let specifier =
        String::from_utf8(read!(specifier_len, "specifier").to_vec())
          .map_err(|_| ParseError::InvalidV2Specifier(read))?;

      let entry_kind = read!(1, "entry kind")[0];
      match entry_kind {
        0 => {
          let source_offset =
            u32::from_be_bytes(read!(4, "source offset").try_into().unwrap());
          let source_len =
            u32::from_be_bytes(read!(4, "source len").try_into().unwrap());
          let source_map_offset = u32::from_be_bytes(
            read!(4, "source map offset").try_into().unwrap(),
          );
          let source_map_len =
            u32::from_be_bytes(read!(4, "source map len").try_into().unwrap());
          let kind = match read!(1, "module kind")[0] {
            0 => ModuleKind::JavaScript,
            1 => ModuleKind::Json,
            2 => ModuleKind::Jsonc,
            3 => ModuleKind::OpaqueData,
            n => return Err(ParseError::InvalidV2ModuleKind(n, read)),
          };
          let source = if source_offset == 0 && source_len == 0 {
            EszipV2SourceSlot::Ready(Arc::new([]))
          } else {
            EszipV2SourceSlot::Pending {
              offset: source_offset as usize,
              length: source_len as usize,
              wakers: vec![],
            }
          };
          let source_map = if source_map_offset == 0 && source_map_len == 0 {
            EszipV2SourceSlot::Ready(Arc::new([]))
          } else {
            EszipV2SourceSlot::Pending {
              offset: source_map_offset as usize,
              length: source_map_len as usize,
              wakers: vec![],
            }
          };
          let module = EszipV2Module::Module {
            kind,
            source,
            source_map,
          };
          modules.insert(specifier, module);
        }
        1 => {
          let target_len =
            u32::from_be_bytes(read!(4, "target len").try_into().unwrap())
              as usize;
          let target = String::from_utf8(read!(target_len, "target").to_vec())
            .map_err(|_| ParseError::InvalidV2Specifier(read))?;
          modules.insert(specifier, EszipV2Module::Redirect { target });
        }
        2 if supports_npm => {
          // npm specifier
          let pkg_id =
            u32::from_be_bytes(read!(4, "npm package id").try_into().unwrap());
          npm_specifiers.insert(specifier, EszipNpmPackageIndex(pkg_id));
        }
        n => return Err(ParseError::InvalidV2EntryKind(n, read)),
      };
    }

    let npm_snapshot = if supports_npm {
      read_npm_section(&mut reader, npm_specifiers).await?
    } else {
      None
    };

    let mut source_offsets = modules
      .iter()
      .filter_map(|(specifier, m)| {
        if let EszipV2Module::Module {
          source: EszipV2SourceSlot::Pending { offset, length, .. },
          ..
        } = m
        {
          Some((*offset, (*length, specifier.clone())))
        } else {
          None
        }
      })
      .collect::<HashMap<_, _>>();

    let mut source_map_offsets = modules
      .iter()
      .filter_map(|(specifier, m)| {
        if let EszipV2Module::Module {
          source_map: EszipV2SourceSlot::Pending { offset, length, .. },
          ..
        } = m
        {
          Some((*offset, (*length, specifier.clone())))
        } else {
          None
        }
      })
      .collect::<HashMap<_, _>>();

    let modules = Arc::new(Mutex::new(modules));
    let modules_ = modules.clone();

    let fut = async move {
      let modules = modules_;

      let sources_len = read_u32(&mut reader).await? as usize;
      let mut read = 0;

      while read < sources_len {
        let (length, specifier) = source_offsets
          .remove(&read)
          .ok_or(ParseError::InvalidV2SourceOffset(read))?;

        let mut source_bytes = vec![0u8; length];
        reader.read_exact(&mut source_bytes).await?;

        // Options::source_digest_size() is always present unless the file
        // uses an unknown hash function without being explicit about the digest size
        // or checksum is disabled (not possible currently)
        if let Some(source_digest_size) = options.source_digest_size() {
          let mut expected_hash = vec![0u8; source_digest_size as usize];
          reader.read_exact(&mut expected_hash).await?;

          // Options::source_hash_function is None when the eszip uses an unknown hash function
          // (forwards compatibility). In that case, degrade to not checking the checksum
          if let Some(source_hash_function) = options.source_hash_function {
            let actual_hash = source_hash_function.hash(&source_bytes);
            if actual_hash != expected_hash {
              return Err(ParseError::InvalidV2SourceHash(specifier));
            }
          }
        }

        read += length + options.source_digest_size().unwrap_or(0) as usize;

        let wakers = {
          let mut modules = modules.lock().unwrap();
          let module = modules.get_mut(&specifier).expect("module not found");
          match module {
            EszipV2Module::Module { ref mut source, .. } => {
              let slot = std::mem::replace(
                source,
                EszipV2SourceSlot::Ready(Arc::from(source_bytes)),
              );

              match slot {
                EszipV2SourceSlot::Pending { wakers, .. } => wakers,
                _ => panic!("already populated source slot"),
              }
            }
            _ => panic!("invalid module type"),
          }
        };
        for w in wakers {
          w.wake();
        }
      }

      let source_maps_len = read_u32(&mut reader).await? as usize;
      let mut read = 0;

      while read < source_maps_len {
        let (length, specifier) = source_map_offsets
          .remove(&read)
          .ok_or(ParseError::InvalidV2SourceOffset(read))?;

        let mut source_map_bytes = vec![0u8; length];
        reader.read_exact(&mut source_map_bytes).await?;

        // Options::source_digest_size() is always present unless the file
        // uses an unknown hash function without being explicit about the digest size
        // or checksum is disabled (not possible currently)
        if let Some(source_digest_size) = options.source_digest_size() {
          let mut expected_hash = vec![0u8; source_digest_size as usize];
          reader.read_exact(&mut expected_hash).await?;

          // Options::source_hash_function is None when the eszip uses an unknown hash function
          // (forwards compatibility). In that case, degrade to not checking the checksum
          if let Some(source_hash_function) = options.source_hash_function {
            let actual_hash = source_hash_function.hash(&source_map_bytes);
            if actual_hash != expected_hash {
              return Err(ParseError::InvalidV2SourceHash(specifier));
            }
          }
        }

        read += length + options.source_digest_size().unwrap_or(0) as usize;

        let wakers = {
          let mut modules = modules.lock().unwrap();
          let module = modules.get_mut(&specifier).expect("module not found");
          match module {
            EszipV2Module::Module {
              ref mut source_map, ..
            } => {
              let slot = std::mem::replace(
                source_map,
                EszipV2SourceSlot::Ready(Arc::from(source_map_bytes)),
              );

              match slot {
                EszipV2SourceSlot::Pending { wakers, .. } => wakers,
                _ => panic!("already populated source_map slot"),
              }
            }
            _ => panic!("invalid module type"),
          }
        };
        for w in wakers {
          w.wake();
        }
      }

      Ok(reader)
    };

    Ok((
      EszipV2 {
        modules: EszipV2Modules(modules),
        npm_snapshot,
        options,
      },
      fut,
    ))
  }

  /// Add an import map to the eszip archive. The import map will always be
  /// placed at the top of the archive, so it can be read before any other
  /// modules are loaded.
  ///
  /// If a module with this specifier is already present, then this is a no-op
  /// (except that this specifier will now be at the top of the archive).
  pub fn add_import_map(
    &mut self,
    kind: ModuleKind,
    specifier: String,
    source: Arc<[u8]>,
  ) {
    debug_assert!(matches!(kind, ModuleKind::Json | ModuleKind::Jsonc));

    let mut modules = self.modules.0.lock().unwrap();

    // If an entry with the specifier already exists, we just move that to the
    // top and return without inserting new source.
    if modules.contains_key(&specifier) {
      modules.to_front(&specifier);
      return;
    }

    modules.insert(
      specifier.clone(),
      EszipV2Module::Module {
        kind,
        source: EszipV2SourceSlot::Ready(source),
        source_map: EszipV2SourceSlot::Ready(Arc::new([])),
      },
    );
    modules.to_front(&specifier);
  }

  /// Add an opaque data to the eszip.
  pub fn add_opaque_data(&mut self, specifier: String, data: Arc<[u8]>) {
    let mut modules = self.modules.0.lock().unwrap();
    modules.insert(
      specifier,
      EszipV2Module::Module {
        kind: ModuleKind::OpaqueData,
        source: EszipV2SourceSlot::Ready(data),
        source_map: EszipV2SourceSlot::Ready(Arc::new([])),
      },
    );
  }

  /// Adds an npm resolution snapshot to the eszip.
  pub fn add_npm_snapshot(
    &mut self,
    snapshot: ValidSerializedNpmResolutionSnapshot,
  ) {
    if !snapshot.as_serialized().packages.is_empty() {
      self.npm_snapshot = Some(snapshot);
    }
  }

  /// Takes an npm resolution snapshot from the eszip.
  pub fn take_npm_snapshot(
    &mut self,
  ) -> Option<ValidSerializedNpmResolutionSnapshot> {
    self.npm_snapshot.take()
  }

  /// Configure the hash function with which to checksum the source of the modules
  ///
  /// Defaults to Sha256.
  pub fn set_source_hash_function(&mut self, hash_function: HashFunction) {
    self.options.source_hash_function = Some(hash_function);
  }

  /// Check if the module's sources have been (or can be) checksumed
  ///
  /// Returns false if the parsed eszip is configured with a hash function that the current version
  /// of the library does not know. In that case, the parsing has continued without checksuming the
  /// module's source, therefore proceed with caution.
  pub fn is_source_checksumed(&self) -> bool {
    self.options.source_hash_function.is_some()
  }

  /// Serialize the eszip archive into a byte buffer.
  pub fn into_bytes(self) -> Vec<u8> {
    fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
      let mut hasher = sha2::Sha256::new();
      hasher.update(bytes);
      hasher.finalize().into()
    }

    fn append_string(bytes: &mut Vec<u8>, string: &str) {
      let len = string.len() as u32;
      bytes.extend_from_slice(&len.to_be_bytes());
      bytes.extend_from_slice(string.as_bytes());
    }

    let (source_hash_function, source_digest_size) = self
      .options
      .source_hash_function
      .zip(self.options.source_digest_size())
      .expect("Disabling checksum should not be possible");

    debug_assert_eq!(
      source_digest_size,
      source_hash_function.digest_size(),
      "customizing the digest size should not be posible"
    );

    let mut options_header = ESZIP_V2_2_MAGIC.to_vec();

    let options_header_length_pos = options_header.len();
    const OPTIONS_HEADER_LENGTH_SIZE: usize = size_of::<u32>();
    options_header.extend_from_slice(&[0; OPTIONS_HEADER_LENGTH_SIZE]); // Reserve for length

    let options_header_start = options_header.len();
    options_header.extend_from_slice(&[0, source_hash_function as u8]);
    options_header.extend_from_slice(&[1, source_digest_size]);

    let options_header_length =
      (options_header.len() - options_header_start) as u32;
    options_header[options_header_length_pos..options_header_start]
      .copy_from_slice(&options_header_length.to_be_bytes());
    let options_header_hash =
      hash_bytes(&options_header[options_header_start..]);
    options_header.extend_from_slice(&options_header_hash);

    let mut modules_header = options_header;
    let modules_header_length_pos = modules_header.len();
    modules_header.extend_from_slice(&[0u8; 4]); // add 4 bytes of space to put the header length in later
    let modules_header_start = modules_header.len();
    let mut npm_bytes: Vec<u8> = Vec::new();
    let mut sources: Vec<u8> = Vec::new();
    let mut source_maps: Vec<u8> = Vec::new();

    let modules = self.modules.0.lock().unwrap();

    for (specifier, module) in modules.iter() {
      append_string(&mut modules_header, specifier);

      match module {
        EszipV2Module::Module {
          kind,
          source,
          source_map,
        } => {
          modules_header.push(HeaderFrameKind::Module as u8);

          // add the source to the `sources` bytes
          let source_bytes = source.bytes();
          let source_length = source_bytes.len() as u32;
          if source_length > 0 {
            let source_offset = sources.len() as u32;
            sources.extend_from_slice(source_bytes);
            sources.extend_from_slice(&source_hash_function.hash(source_bytes));

            modules_header.extend_from_slice(&source_offset.to_be_bytes());
            modules_header.extend_from_slice(&source_length.to_be_bytes());
          } else {
            modules_header.extend_from_slice(&0u32.to_be_bytes());
            modules_header.extend_from_slice(&0u32.to_be_bytes());
          }

          // add the source map to the `source_maps` bytes
          let source_map_bytes = source_map.bytes();
          let source_map_length = source_map_bytes.len() as u32;
          if source_map_length > 0 {
            let source_map_offset = source_maps.len() as u32;
            source_maps.extend_from_slice(source_map_bytes);
            source_maps
              .extend_from_slice(&source_hash_function.hash(source_map_bytes));

            modules_header.extend_from_slice(&source_map_offset.to_be_bytes());
            modules_header.extend_from_slice(&source_map_length.to_be_bytes());
          } else {
            modules_header.extend_from_slice(&0u32.to_be_bytes());
            modules_header.extend_from_slice(&0u32.to_be_bytes());
          }

          // add module kind to the header
          modules_header.push(*kind as u8);
        }
        EszipV2Module::Redirect { target } => {
          modules_header.push(HeaderFrameKind::Redirect as u8);
          let target_bytes = target.as_bytes();
          let target_length = target_bytes.len() as u32;
          modules_header.extend_from_slice(&target_length.to_be_bytes());
          modules_header.extend_from_slice(target_bytes);
        }
      }
    }

    // add npm snapshot entries to the header and fill the npm bytes
    if let Some(npm_snapshot) = self.npm_snapshot {
      let npm_snapshot = npm_snapshot.into_serialized();
      let ids_to_eszip_ids = npm_snapshot
        .packages
        .iter()
        .enumerate()
        .map(|(i, pkg)| (&pkg.id, i as u32))
        .collect::<HashMap<_, _>>();

      let mut root_packages: Vec<_> =
        npm_snapshot.root_packages.into_iter().collect();
      root_packages.sort();
      for (req, id) in root_packages {
        append_string(&mut modules_header, &req.to_string());
        modules_header.push(HeaderFrameKind::NpmSpecifier as u8);
        let id = ids_to_eszip_ids.get(&id).unwrap();
        modules_header.extend_from_slice(&id.to_be_bytes());
      }

      for pkg in &npm_snapshot.packages {
        append_string(&mut npm_bytes, &pkg.id.as_serialized());
        let deps_len = pkg.dependencies.len() as u32;
        npm_bytes.extend_from_slice(&deps_len.to_be_bytes());
        let mut deps: Vec<_> = pkg
          .dependencies
          .iter()
          .map(|(a, b)| (a.clone(), b.clone()))
          .collect();
        deps.sort();
        for (req, id) in deps {
          append_string(&mut npm_bytes, &req.to_string());
          let id = ids_to_eszip_ids.get(&id).unwrap();
          npm_bytes.extend_from_slice(&id.to_be_bytes());
        }
      }
    }

    // populate header length
    let modules_header_length =
      (modules_header.len() - modules_header_start) as u32;
    modules_header[modules_header_length_pos..modules_header_start]
      .copy_from_slice(&modules_header_length.to_be_bytes());

    // add header hash
    let modules_header_bytes = &modules_header[modules_header_start..];
    modules_header.extend_from_slice(&hash_bytes(modules_header_bytes));

    let mut bytes = modules_header;

    let npm_bytes_len = npm_bytes.len() as u32;
    bytes.extend_from_slice(&npm_bytes_len.to_be_bytes());
    bytes.extend_from_slice(&npm_bytes);
    bytes.extend_from_slice(&hash_bytes(&npm_bytes));

    // add sources
    let sources_len = sources.len() as u32;
    bytes.extend_from_slice(&sources_len.to_be_bytes());
    bytes.extend_from_slice(&sources);

    let source_maps_len = source_maps.len() as u32;
    bytes.extend_from_slice(&source_maps_len.to_be_bytes());
    bytes.extend_from_slice(&source_maps);

    bytes
  }

  /// Turn a [deno_graph::ModuleGraph] into an [EszipV2]. All modules from the
  /// graph will be transpiled and stored in the eszip archive.
  ///
  /// The ordering of the modules in the graph is dependant on the module graph
  /// tree. The root module is added to the top of the archive, and the leaves
  /// to the end. This allows for efficient deserialization of the archive right
  /// into an isolate.
  pub fn from_graph(
    graph: ModuleGraph,
    parser: &CapturingModuleParser,
    transpile_options: TranspileOptions,
    mut emit_options: EmitOptions,
  ) -> Result<Self, anyhow::Error> {
    emit_options.inline_sources = true;
    if emit_options.source_map == SourceMapOption::Inline {
      emit_options.source_map = SourceMapOption::Separate;
    }

    let mut modules = LinkedHashMap::new();

    fn visit_module(
      graph: &ModuleGraph,
      parser: &CapturingModuleParser,
      transpile_options: &TranspileOptions,
      emit_options: &EmitOptions,
      modules: &mut LinkedHashMap<String, EszipV2Module>,
      specifier: &Url,
      is_dynamic: bool,
    ) -> Result<(), anyhow::Error> {
      let module = match graph.try_get(specifier) {
        Ok(Some(module)) => module,
        Ok(None) => {
          return Err(anyhow::anyhow!("module not found {}", specifier));
        }
        Err(err) => {
          if is_dynamic {
            // dynamic imports are allowed to fail
            return Ok(());
          }
          return Err(anyhow::anyhow!(
            "failed to load '{}': {}",
            specifier,
            err
          ));
        }
      };

      let specifier = module.specifier().as_str();
      if modules.contains_key(specifier) {
        return Ok(());
      }

      match module {
        deno_graph::Module::Js(module) => {
          let source: Arc<[u8]>;
          let source_map: Arc<[u8]>;
          match module.media_type {
            deno_graph::MediaType::JavaScript | deno_graph::MediaType::Mjs => {
              source = Arc::from(module.source.clone());
              source_map = Arc::new( []);
            }
            deno_graph::MediaType::Jsx
            | deno_graph::MediaType::TypeScript
            | deno_graph::MediaType::Mts
            | deno_graph::MediaType::Tsx
            | deno_graph::MediaType::Dts
            | deno_graph::MediaType::Dmts => {
              let parsed_source = parser.parse_module(ParseOptions {
                specifier: &module.specifier,
                source: module.source.clone(),
                media_type: module.media_type,
                scope_analysis: false,
              })?;
              let EmittedSource {
                text,
                source_map: maybe_source_map,
              } = parsed_source.transpile(transpile_options, emit_options)?.into_source();
              source = Arc::from(text.into_bytes());
              source_map = Arc::from(maybe_source_map.unwrap_or_default().into_bytes());
            }
            _ => {
              return Err(anyhow::anyhow!(
                "unsupported media type {} for {}",
                module.media_type,
                specifier
              ));
            }
          };

          let specifier = module.specifier.to_string();
          let eszip_module = EszipV2Module::Module {
            kind: ModuleKind::JavaScript,
            source: EszipV2SourceSlot::Ready(source),
            source_map: EszipV2SourceSlot::Ready(source_map),
          };
          modules.insert(specifier, eszip_module);

          // now walk the code dependencies
          for dep in module.dependencies.values() {
            if let Some(specifier) = dep.get_code() {
              visit_module(
                graph,
                parser,
                transpile_options,
                emit_options,
                modules,
                specifier,
                dep.is_dynamic,
              )?;
            }
          }

          Ok(())
        }
        deno_graph::Module::Json(module) => {
          let specifier = module.specifier.to_string();
          let eszip_module = EszipV2Module::Module {
            kind: ModuleKind::Json,
            source: EszipV2SourceSlot::Ready( module.source.clone().into()),
            source_map: EszipV2SourceSlot::Ready(Arc::new([])),
          };
          modules.insert(specifier, eszip_module);
          Ok(())
        }
        deno_graph::Module::External(_)
        // we ignore any npm modules found in the graph and instead
        // rely solely on the npm snapshot for this information
        | deno_graph::Module::Npm(_)
        | deno_graph::Module::Node(_) => Ok(()),
      }
    }

    for root in &graph.roots {
      visit_module(
        &graph,
        parser,
        &transpile_options,
        &emit_options,
        &mut modules,
        root,
        false,
      )?;
    }

    for (specifier, target) in &graph.redirects {
      let module = EszipV2Module::Redirect {
        target: target.to_string(),
      };
      modules.insert(specifier.to_string(), module);
    }

    Ok(Self {
      modules: EszipV2Modules(Arc::new(Mutex::new(modules))),
      npm_snapshot: None,
      options: Options::default(),
    })
  }

  /// Get the module metadata for a given module specifier. This function will
  /// follow redirects. The returned module has functions that can be used to
  /// obtain the module source and source map. The module returned from this
  /// function is guaranteed to be a valid module, which can be loaded into v8.
  ///
  /// Note that this function should be used to obtain a module; if you wish to
  /// get an import map, use [`get_import_map`](Self::get_import_map) instead.
  pub fn get_module(&self, specifier: &str) -> Option<Module> {
    let module = self.lookup(specifier)?;

    // JSONC is contained in this eszip only for use as an import map. In
    // order for the caller to get this JSONC, call `get_import_map` instead.
    if module.kind == ModuleKind::Jsonc {
      return None;
    }

    Some(module)
  }

  /// Get the import map for a given specifier.
  ///
  /// Note that this function should be used to obtain an import map; the returned
  /// "Module" is not necessarily a valid module that can be loaded into v8 (in
  /// other words, JSONC may be returned). If you wish to get a valid module,
  /// use [`get_module`](Self::get_module) instead.
  pub fn get_import_map(&self, specifier: &str) -> Option<Module> {
    let import_map = self.lookup(specifier)?;

    // Import map must be either JSON or JSONC (but JSONC is a special case;
    // it's allowed when embedded in a Deno's config file)
    if import_map.kind == ModuleKind::JavaScript {
      return None;
    }

    Some(import_map)
  }

  fn lookup(&self, specifier: &str) -> Option<Module> {
    let mut specifier = specifier;
    let mut visited = HashSet::new();
    let modules = self.modules.0.lock().unwrap();
    loop {
      visited.insert(specifier);
      let module = modules.get(specifier)?;
      match module {
        EszipV2Module::Module { kind, .. } => {
          return Some(Module {
            specifier: specifier.to_string(),
            kind: *kind,
            inner: ModuleInner::V2(self.modules.clone()),
          });
        }
        EszipV2Module::Redirect { ref target } => {
          specifier = target;
          if visited.contains(specifier) {
            return None;
          }
        }
      }
    }
  }

  /// Returns a list of all the module specifiers in this eszip archive.
  pub fn specifiers(&self) -> Vec<String> {
    let modules = self.modules.0.lock().unwrap();
    modules.keys().cloned().collect()
  }
}

/// Get an iterator over all the modules (including an import map, if any) in
/// this eszip archive.
///
/// Note that the iterator will iterate over the specifiers' "snapshot" of the
/// archive. If a new module is added to the archive after the iterator is
/// created via `into_iter()`, that module will not be iterated over.
impl IntoIterator for EszipV2 {
  type Item = (String, Module);
  type IntoIter = std::vec::IntoIter<Self::Item>;

  fn into_iter(self) -> Self::IntoIter {
    let specifiers = self.specifiers();
    let mut v = Vec::with_capacity(specifiers.len());
    for specifier in specifiers {
      let Some(module) = self.lookup(&specifier) else {
        continue;
      };
      v.push((specifier, module));
    }

    v.into_iter()
  }
}

async fn read_npm_section<R: futures::io::AsyncRead + Unpin>(
  reader: &mut futures::io::BufReader<R>,
  npm_specifiers: HashMap<String, EszipNpmPackageIndex>,
) -> Result<Option<ValidSerializedNpmResolutionSnapshot>, ParseError> {
  let snapshot = HashedSection::read(reader).await?;
  if !snapshot.hash_valid() {
    return Err(ParseError::InvalidV2NpmSnapshotHash);
  }
  let original_bytes = snapshot.bytes();
  if original_bytes.is_empty() {
    return Ok(None);
  }
  let mut packages = Vec::new();
  let mut bytes = original_bytes;
  while !bytes.is_empty() {
    let result = EszipNpmModule::parse(bytes).map_err(|err| {
      let offset = original_bytes.len() - bytes.len();
      ParseError::InvalidV2NpmPackageOffset(offset, err)
    })?;
    bytes = result.0;
    packages.push(result.1);
  }
  let mut pkg_index_to_pkg_id = HashMap::with_capacity(packages.len());
  for (i, pkg) in packages.iter().enumerate() {
    let id = NpmPackageId::from_serialized(&pkg.name).map_err(|err| {
      ParseError::InvalidV2NpmPackage(pkg.name.clone(), err.into())
    })?;
    pkg_index_to_pkg_id.insert(EszipNpmPackageIndex(i as u32), id);
  }
  let mut final_packages = Vec::with_capacity(packages.len());
  for (i, pkg) in packages.into_iter().enumerate() {
    let eszip_id = EszipNpmPackageIndex(i as u32);
    let id = pkg_index_to_pkg_id.get(&eszip_id).unwrap();
    let mut dependencies = HashMap::with_capacity(pkg.dependencies.len());
    for (key, pkg_index) in pkg.dependencies {
      let id = match pkg_index_to_pkg_id.get(&pkg_index) {
        Some(id) => id,
        None => {
          return Err(ParseError::InvalidV2NpmPackage(
            pkg.name,
            anyhow::anyhow!("missing index '{}'", pkg_index.0),
          ));
        }
      };
      dependencies.insert(key, id.clone());
    }
    final_packages.push(SerializedNpmResolutionSnapshotPackage {
      id: id.clone(),
      system: Default::default(),
      dist: Default::default(),
      dependencies,
      optional_dependencies: Default::default(),
      bin: None,
    });
  }
  let mut root_packages = HashMap::with_capacity(npm_specifiers.len());
  for (req, pkg_index) in npm_specifiers {
    let id = match pkg_index_to_pkg_id.get(&pkg_index) {
      Some(id) => id,
      None => {
        return Err(ParseError::InvalidV2NpmPackageReq(
          req,
          anyhow::anyhow!("missing index '{}'", pkg_index.0),
        ));
      }
    };
    let req = PackageReq::from_str(&req)
      .map_err(|err| ParseError::InvalidV2NpmPackageReq(req, err.into()))?;
    root_packages.insert(req, id.clone());
  }
  Ok(Some(
    SerializedNpmResolutionSnapshot {
      packages: final_packages,
      root_packages,
    }
    // this is ok because we have already verified that all the
    // identifiers found in the snapshot are valid via the
    // eszip npm package id -> npm package id mapping
    .into_valid_unsafe(),
  ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct EszipNpmPackageIndex(u32);

impl EszipNpmPackageIndex {
  pub fn parse(input: &[u8]) -> std::io::Result<(&[u8], Self)> {
    let (input, pkg_index) = parse_u32(input)?;
    Ok((input, EszipNpmPackageIndex(pkg_index)))
  }
}

struct EszipNpmModule {
  name: String,
  dependencies: HashMap<String, EszipNpmPackageIndex>,
}

impl EszipNpmModule {
  pub fn parse(input: &[u8]) -> std::io::Result<(&[u8], EszipNpmModule)> {
    let (input, name) = parse_string(input)?;
    let (input, dep_size) = parse_u32(input)?;
    let mut deps = HashMap::with_capacity(dep_size as usize);
    let mut input = input;
    for _ in 0..dep_size {
      let parsed_dep = EszipNpmDependency::parse(input)?;
      input = parsed_dep.0;
      let dep = parsed_dep.1;
      deps.insert(dep.0, dep.1);
    }
    Ok((
      input,
      EszipNpmModule {
        name,
        dependencies: deps,
      },
    ))
  }
}

struct EszipNpmDependency(String, EszipNpmPackageIndex);

impl EszipNpmDependency {
  pub fn parse(input: &[u8]) -> std::io::Result<(&[u8], Self)> {
    let (input, name) = parse_string(input)?;
    let (input, pkg_index) = EszipNpmPackageIndex::parse(input)?;
    Ok((input, EszipNpmDependency(name, pkg_index)))
  }
}

fn parse_string(input: &[u8]) -> std::io::Result<(&[u8], String)> {
  let (input, size) = parse_u32(input)?;
  let (input, name) = move_bytes(input, size as usize)?;
  let text = String::from_utf8(name.to_vec()).map_err(|_| {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid utf-8 data")
  })?;
  Ok((input, text))
}

fn parse_u32(input: &[u8]) -> std::io::Result<(&[u8], u32)> {
  let (input, value_bytes) = move_bytes(input, 4)?;
  let value = u32::from_be_bytes(value_bytes.try_into().unwrap());
  Ok((input, value))
}

fn move_bytes(
  bytes: &[u8],
  len: usize,
) -> Result<(&[u8], &[u8]), std::io::Error> {
  if bytes.len() < len {
    Err(std::io::Error::new(
      std::io::ErrorKind::UnexpectedEof,
      "unexpected end of bytes",
    ))
  } else {
    Ok((&bytes[len..], &bytes[..len]))
  }
}

struct HashedSection(Vec<u8>);

impl HashedSection {
  /// Reads a section that's defined as:
  ///   Size (4) | Body (n) | Hash (32)
  async fn read<R: futures::io::AsyncRead + Unpin>(
    reader: &mut futures::io::BufReader<R>,
  ) -> Result<HashedSection, ParseError> {
    let len = read_u32(reader).await? as usize;
    HashedSection::read_with_size(reader, len).await
  }

  /// Reads a section that's defined as:
  ///   Body (n) | Hash (32)
  /// Where the `n` size is provided.
  async fn read_with_size<R: futures::io::AsyncRead + Unpin>(
    reader: &mut futures::io::BufReader<R>,
    len: usize,
  ) -> Result<HashedSection, ParseError> {
    let mut body_and_hash = vec![0u8; len + 32];
    reader.read_exact(&mut body_and_hash).await?;

    Ok(HashedSection(body_and_hash))
  }

  fn bytes(&self) -> &[u8] {
    &self.0[..self.len()]
  }

  fn len(&self) -> usize {
    self.0.len() - 32
  }

  fn hash(&self) -> &[u8] {
    &self.0[self.len()..]
  }

  fn hash_valid(&self) -> bool {
    let mut hasher = Sha256::new();
    hasher.update(self.bytes());
    let actual_hash = hasher.finalize();
    let expected_hash = self.hash();
    &*actual_hash == expected_hash
  }
}

async fn read_u32<R: futures::io::AsyncRead + Unpin>(
  reader: &mut futures::io::BufReader<R>,
) -> Result<u32, ParseError> {
  let mut buf = [0u8; 4];
  reader.read_exact(&mut buf).await?;
  Ok(u32::from_be_bytes(buf))
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;
  use std::io::Cursor;
  use std::path::Path;
  use std::sync::Arc;

  use deno_ast::EmitOptions;
  use deno_ast::TranspileOptions;
  use deno_graph::source::CacheSetting;
  use deno_graph::source::LoadOptions;
  use deno_graph::source::LoadResponse;
  use deno_graph::source::ResolveError;
  use deno_graph::BuildOptions;
  use deno_graph::CapturingModuleAnalyzer;
  use deno_graph::GraphKind;
  use deno_graph::ModuleGraph;
  use deno_graph::ModuleSpecifier;
  use deno_npm::resolution::SerializedNpmResolutionSnapshot;
  use deno_npm::resolution::SerializedNpmResolutionSnapshotPackage;
  use deno_npm::NpmPackageId;
  use deno_semver::package::PackageReq;
  use futures::io::AllowStdIo;
  use futures::io::BufReader;
  use import_map::ImportMap;
  use pretty_assertions::assert_eq;
  use url::Url;

  use crate::EszipV2;
  use crate::ModuleKind;

  struct FileLoader {
    base_dir: String,
  }

  macro_rules! assert_matches_file {
    ($source:ident, $file:literal) => {
      assert_eq!(
        String::from_utf8($source.to_vec()).unwrap(),
        include_str!($file)
      );
    };
  }

  impl deno_graph::source::Loader for FileLoader {
    fn load(
      &self,
      specifier: &ModuleSpecifier,
      _options: LoadOptions,
    ) -> deno_graph::source::LoadFuture {
      match specifier.scheme() {
        "file" => {
          let path = format!("{}{}", self.base_dir, specifier.path());
          Box::pin(async move {
            let path = Path::new(&path);
            let Ok(resolved) = path.canonicalize() else {
              return Ok(None);
            };
            let source = std::fs::read(&resolved).unwrap();
            let specifier =
              resolved.file_name().unwrap().to_string_lossy().to_string();
            let specifier =
              Url::parse(&format!("file:///{specifier}")).unwrap();
            Ok(Some(LoadResponse::Module {
              content: source.into(),
              maybe_headers: None,
              specifier,
            }))
          })
        }
        "data" => {
          let result = deno_graph::source::load_data_url(specifier);
          Box::pin(async move { result })
        }
        _ => unreachable!(),
      }
    }
  }

  #[derive(Debug)]
  struct ImportMapResolver(ImportMap);

  impl deno_graph::source::Resolver for ImportMapResolver {
    fn resolve(
      &self,
      specifier: &str,
      referrer_range: &deno_graph::Range,
      _mode: deno_graph::source::ResolutionMode,
    ) -> Result<ModuleSpecifier, ResolveError> {
      self
        .0
        .resolve(specifier, &referrer_range.specifier)
        .map_err(|err| ResolveError::Other(err.into()))
    }
  }

  #[tokio::test]
  async fn test_graph_external() {
    let roots = vec![ModuleSpecifier::parse("file:///external.ts").unwrap()];

    struct ExternalLoader;

    impl deno_graph::source::Loader for ExternalLoader {
      fn load(
        &self,
        specifier: &ModuleSpecifier,
        options: LoadOptions,
      ) -> deno_graph::source::LoadFuture {
        if options.is_dynamic {
          unreachable!();
        }
        let scheme = specifier.scheme();
        if scheme == "extern" {
          let specifier = specifier.clone();
          return Box::pin(async move {
            Ok(Some(LoadResponse::External { specifier }))
          });
        }
        assert_eq!(scheme, "file");
        let path = format!("./src/testdata/source{}", specifier.path());
        Box::pin(async move {
          let path = Path::new(&path);
          let resolved = path.canonicalize().unwrap();
          let source = std::fs::read(&resolved).unwrap();
          let specifier =
            resolved.file_name().unwrap().to_string_lossy().to_string();
          let specifier = Url::parse(&format!("file:///{specifier}")).unwrap();
          Ok(Some(LoadResponse::Module {
            content: source.into(),
            maybe_headers: None,
            specifier,
          }))
        })
      }
    }

    let analyzer = CapturingModuleAnalyzer::default();
    let mut graph = ModuleGraph::new(GraphKind::CodeOnly);
    graph
      .build(
        roots,
        &ExternalLoader,
        BuildOptions {
          module_analyzer: &analyzer,
          ..Default::default()
        },
      )
      .await;
    graph.valid().unwrap();
    let eszip = super::EszipV2::from_graph(
      graph,
      &analyzer.as_capturing_parser(),
      TranspileOptions::default(),
      EmitOptions::default(),
    )
    .unwrap();
    let module = eszip.get_module("file:///external.ts").unwrap();
    assert_eq!(module.specifier, "file:///external.ts");
    assert!(eszip.get_module("external:fs").is_none());
  }

  #[tokio::test]
  async fn from_graph_redirect() {
    let roots = vec![ModuleSpecifier::parse("file:///main.ts").unwrap()];
    let analyzer = CapturingModuleAnalyzer::default();
    let mut graph = ModuleGraph::new(GraphKind::CodeOnly);
    let loader = FileLoader {
      base_dir: "./src/testdata/source".to_string(),
    };
    graph
      .build(
        roots,
        &loader,
        BuildOptions {
          module_analyzer: &analyzer,
          ..Default::default()
        },
      )
      .await;
    graph.valid().unwrap();
    let eszip = super::EszipV2::from_graph(
      graph,
      &analyzer.as_capturing_parser(),
      TranspileOptions::default(),
      EmitOptions::default(),
    )
    .unwrap();
    let module = eszip.get_module("file:///main.ts").unwrap();
    assert_eq!(module.specifier, "file:///main.ts");
    let source = module.source().await.unwrap();
    assert_matches_file!(source, "./testdata/emit/main.ts");
    let source_map = module.source_map().await.unwrap();
    assert_matches_file!(source_map, "./testdata/emit/main.ts.map");
    assert_eq!(module.kind, ModuleKind::JavaScript);
    let module = eszip.get_module("file:///a.ts").unwrap();
    assert_eq!(module.specifier, "file:///b.ts");
    let source = module.source().await.unwrap();
    assert_matches_file!(source, "./testdata/emit/b.ts");
    let source_map = module.source_map().await.unwrap();
    assert_matches_file!(source_map, "./testdata/emit/b.ts.map");
    assert_eq!(module.kind, ModuleKind::JavaScript);
  }

  #[tokio::test]
  async fn from_graph_json() {
    let roots = vec![ModuleSpecifier::parse("file:///json.ts").unwrap()];
    let analyzer = CapturingModuleAnalyzer::default();
    let mut graph = ModuleGraph::new(GraphKind::CodeOnly);
    let loader = FileLoader {
      base_dir: "./src/testdata/source".to_string(),
    };
    graph
      .build(
        roots,
        &loader,
        BuildOptions {
          module_analyzer: &analyzer,
          ..Default::default()
        },
      )
      .await;
    graph.valid().unwrap();
    let eszip = super::EszipV2::from_graph(
      graph,
      &analyzer.as_capturing_parser(),
      TranspileOptions::default(),
      EmitOptions::default(),
    )
    .unwrap();
    let module = eszip.get_module("file:///json.ts").unwrap();
    assert_eq!(module.specifier, "file:///json.ts");
    let source = module.source().await.unwrap();
    assert_matches_file!(source, "./testdata/emit/json.ts");
    let _source_map = module.source_map().await.unwrap();
    assert_eq!(module.kind, ModuleKind::JavaScript);
    let module = eszip.get_module("file:///data.json").unwrap();
    assert_eq!(module.specifier, "file:///data.json");
    let source = module.source().await.unwrap();
    assert_matches_file!(source, "./testdata/source/data.json");
    let source_map = module.source_map().await.unwrap();
    assert_eq!(&*source_map, &[0; 0]);
    assert_eq!(module.kind, ModuleKind::Json);
  }

  #[tokio::test]
  async fn from_graph_dynamic() {
    let roots = vec![ModuleSpecifier::parse("file:///dynamic.ts").unwrap()];
    let analyzer = CapturingModuleAnalyzer::default();
    let mut graph = ModuleGraph::new(GraphKind::CodeOnly);
    let loader = FileLoader {
      base_dir: "./src/testdata/source".to_string(),
    };
    graph
      .build(
        roots,
        &loader,
        BuildOptions {
          module_analyzer: &analyzer,
          ..Default::default()
        },
      )
      .await;
    graph.valid().unwrap();
    let eszip = super::EszipV2::from_graph(
      graph,
      &analyzer.as_capturing_parser(),
      TranspileOptions::default(),
      EmitOptions::default(),
    )
    .unwrap();
    let module = eszip.get_module("file:///dynamic.ts").unwrap();
    assert_eq!(module.specifier, "file:///dynamic.ts");
    let source = module.source().await.unwrap();
    assert_matches_file!(source, "./testdata/emit/dynamic.ts");
    let _source_map = module.source_map().await.unwrap();
    assert_eq!(module.kind, ModuleKind::JavaScript);
    let module = eszip.get_module("file:///data.json");
    assert!(module.is_some()); // we include statically analyzable dynamic imports
    let mut specifiers = eszip.specifiers();
    specifiers.sort();
    assert_eq!(specifiers, vec!["file:///data.json", "file:///dynamic.ts"]);
  }

  #[tokio::test]
  async fn from_graph_dynamic_data() {
    let roots =
      vec![ModuleSpecifier::parse("file:///dynamic_data.ts").unwrap()];
    let analyzer = CapturingModuleAnalyzer::default();
    let mut graph = ModuleGraph::new(GraphKind::CodeOnly);
    let loader = FileLoader {
      base_dir: "./src/testdata/source".to_string(),
    };
    graph
      .build(
        roots,
        &loader,
        BuildOptions {
          module_analyzer: &analyzer,
          ..Default::default()
        },
      )
      .await;
    graph.valid().unwrap();
    let eszip = super::EszipV2::from_graph(
      graph,
      &analyzer.as_capturing_parser(),
      TranspileOptions::default(),
      EmitOptions::default(),
    )
    .unwrap();
    let module = eszip.get_module("file:///dynamic_data.ts").unwrap();
    assert_eq!(module.specifier, "file:///dynamic_data.ts");
    let source = module.source().await.unwrap();
    assert_matches_file!(source, "./testdata/emit/dynamic_data.ts");
  }

  #[tokio::test]
  async fn file_format_parse_redirect() {
    let file = std::fs::File::open("./src/testdata/redirect.eszip2").unwrap();
    let (eszip, fut) =
      super::EszipV2::parse(BufReader::new(AllowStdIo::new(file)))
        .await
        .unwrap();

    let test = async move {
      let module = eszip.get_module("file:///main.ts").unwrap();
      assert_eq!(module.specifier, "file:///main.ts");
      let source = module.source().await.unwrap();
      assert_matches_file!(source, "./testdata/redirect_data/main.ts");
      let source_map = module.source_map().await.unwrap();
      assert_matches_file!(source_map, "./testdata/redirect_data/main.ts.map");
      assert_eq!(module.kind, ModuleKind::JavaScript);
      let module = eszip.get_module("file:///a.ts").unwrap();
      assert_eq!(module.specifier, "file:///b.ts");
      let source = module.source().await.unwrap();
      assert_matches_file!(source, "./testdata/redirect_data/b.ts");
      let source_map = module.source_map().await.unwrap();
      assert_matches_file!(source_map, "./testdata/redirect_data/b.ts.map");
      assert_eq!(module.kind, ModuleKind::JavaScript);

      Ok(())
    };

    tokio::try_join!(fut, test).unwrap();
  }

  #[tokio::test]
  async fn file_format_parse_json() {
    let file = std::fs::File::open("./src/testdata/json.eszip2").unwrap();
    let (eszip, fut) =
      super::EszipV2::parse(BufReader::new(AllowStdIo::new(file)))
        .await
        .unwrap();

    let test = async move {
      let module = eszip.get_module("file:///json.ts").unwrap();
      assert_eq!(module.specifier, "file:///json.ts");
      let source = module.source().await.unwrap();
      assert_matches_file!(source, "./testdata/source/json.ts");
      let _source_map = module.source_map().await.unwrap();
      assert_eq!(module.kind, ModuleKind::JavaScript);
      let module = eszip.get_module("file:///data.json").unwrap();
      assert_eq!(module.specifier, "file:///data.json");
      let source = module.source().await.unwrap();
      assert_matches_file!(source, "./testdata/emit/data.json");
      let source_map = module.source_map().await.unwrap();
      assert_eq!(&*source_map, &[0; 0]);
      assert_eq!(module.kind, ModuleKind::Json);

      Ok(())
    };

    tokio::try_join!(fut, test).unwrap();
  }

  #[tokio::test]
  async fn file_format_roundtrippable() {
    let file = std::fs::File::open("./src/testdata/redirect.eszip2").unwrap();
    let (eszip, fut) =
      super::EszipV2::parse(BufReader::new(AllowStdIo::new(file)))
        .await
        .unwrap();
    fut.await.unwrap();
    let bytes = eszip.into_bytes();
    insta::assert_debug_snapshot!(bytes);
    let cursor = Cursor::new(bytes);
    let (eszip, fut) =
      super::EszipV2::parse(BufReader::new(AllowStdIo::new(cursor)))
        .await
        .unwrap();
    fut.await.unwrap();
    let module = eszip.get_module("file:///main.ts").unwrap();
    assert_eq!(module.specifier, "file:///main.ts");
    let source = module.source().await.unwrap();
    assert_matches_file!(source, "./testdata/redirect_data/main.ts");
    let source_map = module.source_map().await.unwrap();
    assert_matches_file!(source_map, "./testdata/redirect_data/main.ts.map");
    assert_eq!(module.kind, ModuleKind::JavaScript);
    let module = eszip.get_module("file:///a.ts").unwrap();
    assert_eq!(module.specifier, "file:///b.ts");
    let source = module.source().await.unwrap();
    assert_matches_file!(source, "./testdata/redirect_data/b.ts");
    let source_map = module.source_map().await.unwrap();
    assert_matches_file!(source_map, "./testdata/redirect_data/b.ts.map");
    assert_eq!(module.kind, ModuleKind::JavaScript);
  }

  #[tokio::test]
  async fn import_map() {
    let loader = FileLoader {
      base_dir: "./src/testdata/source".to_string(),
    };
    let resp = deno_graph::source::Loader::load(
      &loader,
      &Url::parse("file:///import_map.json").unwrap(),
      LoadOptions {
        is_dynamic: false,
        cache_setting: CacheSetting::Use,
        maybe_checksum: None,
      },
    )
    .await
    .unwrap()
    .unwrap();
    let (specifier, content) = match resp {
      deno_graph::source::LoadResponse::Module {
        specifier, content, ..
      } => (specifier, content),
      _ => unimplemented!(),
    };
    let import_map = import_map::parse_from_json(
      &specifier,
      &String::from_utf8(content.to_vec()).unwrap(),
    )
    .unwrap();
    let roots = vec![ModuleSpecifier::parse("file:///mapped.js").unwrap()];
    let analyzer = CapturingModuleAnalyzer::default();
    let mut graph = ModuleGraph::new(GraphKind::CodeOnly);
    graph
      .build(
        roots,
        &loader,
        BuildOptions {
          resolver: Some(&ImportMapResolver(import_map.import_map)),
          module_analyzer: &analyzer,
          ..Default::default()
        },
      )
      .await;
    graph.valid().unwrap();
    let mut eszip = super::EszipV2::from_graph(
      graph,
      &analyzer.as_capturing_parser(),
      TranspileOptions::default(),
      EmitOptions::default(),
    )
    .unwrap();
    eszip.add_import_map(ModuleKind::Json, specifier.to_string(), content);

    let module = eszip.get_module("file:///import_map.json").unwrap();
    assert_eq!(module.specifier, "file:///import_map.json");
    let source = module.source().await.unwrap();
    assert_matches_file!(source, "./testdata/source/import_map.json");
    let source_map = module.source_map().await.unwrap();
    assert_eq!(&*source_map, &[0; 0]);
    assert_eq!(module.kind, ModuleKind::Json);

    let module = eszip.get_module("file:///mapped.js").unwrap();
    assert_eq!(module.specifier, "file:///mapped.js");
    let source = module.source().await.unwrap();
    assert_matches_file!(source, "./testdata/source/mapped.js");
    let source_map = module.source_map().await.unwrap();
    assert_eq!(&*source_map, &[0; 0]);
    assert_eq!(module.kind, ModuleKind::JavaScript);

    let module = eszip.get_module("file:///a.ts").unwrap();
    assert_eq!(module.specifier, "file:///b.ts");
    let source = module.source().await.unwrap();
    assert_matches_file!(source, "./testdata/emit/b.ts");
    let source_map = module.source_map().await.unwrap();
    assert_matches_file!(source_map, "./testdata/emit/b.ts.map");
    assert_eq!(module.kind, ModuleKind::JavaScript);
  }

  // https://github.com/denoland/eszip/issues/110
  #[tokio::test]
  async fn import_map_imported_from_program() {
    let loader = FileLoader {
      base_dir: "./src/testdata/source".to_string(),
    };
    let resp = deno_graph::source::Loader::load(
      &loader,
      &Url::parse("file:///import_map.json").unwrap(),
      LoadOptions {
        is_dynamic: false,
        cache_setting: CacheSetting::Use,
        maybe_checksum: None,
      },
    )
    .await
    .unwrap()
    .unwrap();
    let (specifier, content) = match resp {
      deno_graph::source::LoadResponse::Module {
        specifier, content, ..
      } => (specifier, content),
      _ => unimplemented!(),
    };
    let import_map = import_map::parse_from_json(
      &specifier,
      &String::from_utf8(content.to_vec()).unwrap(),
    )
    .unwrap();
    let roots =
      // This file imports `import_map.json` as a module.
      vec![ModuleSpecifier::parse("file:///import_import_map.js").unwrap()];
    let analyzer = CapturingModuleAnalyzer::default();
    let mut graph = ModuleGraph::new(GraphKind::CodeOnly);
    graph
      .build(
        roots,
        &loader,
        BuildOptions {
          resolver: Some(&ImportMapResolver(import_map.import_map)),
          module_analyzer: &analyzer,
          ..Default::default()
        },
      )
      .await;
    graph.valid().unwrap();
    let mut eszip = super::EszipV2::from_graph(
      graph,
      &analyzer.as_capturing_parser(),
      TranspileOptions::default(),
      EmitOptions::default(),
    )
    .unwrap();
    eszip.add_import_map(ModuleKind::Json, specifier.to_string(), content);

    // Verify that the resulting eszip consists of two unique modules even
    // though `import_map.json` is referenced twice:
    // 1. imported from JS
    // 2. specified as the import map
    assert_eq!(
      eszip.specifiers(),
      vec![
        "file:///import_map.json".to_string(),
        "file:///import_import_map.js".to_string(),
      ]
    );
  }

  #[tokio::test]
  async fn deno_jsonc_as_import_map() {
    let loader = FileLoader {
      base_dir: "./src/testdata/deno_jsonc_as_import_map".to_string(),
    };
    let resp = deno_graph::source::Loader::load(
      &loader,
      &Url::parse("file:///deno.jsonc").unwrap(),
      LoadOptions {
        is_dynamic: false,
        cache_setting: CacheSetting::Use,
        maybe_checksum: None,
      },
    )
    .await
    .unwrap()
    .unwrap();
    let (specifier, content) = match resp {
      deno_graph::source::LoadResponse::Module {
        specifier, content, ..
      } => (specifier, content),
      _ => unimplemented!(),
    };
    let import_map = import_map::parse_from_value(
      specifier.clone(),
      jsonc_parser::parse_to_serde_value(
        &String::from_utf8(content.to_vec()).unwrap(),
        &Default::default(),
      )
      .unwrap()
      .unwrap(),
    )
    .unwrap();
    let roots = vec![ModuleSpecifier::parse("file:///main.ts").unwrap()];
    let analyzer = CapturingModuleAnalyzer::default();
    let mut graph = ModuleGraph::new(GraphKind::CodeOnly);
    graph
      .build(
        roots,
        &loader,
        BuildOptions {
          resolver: Some(&ImportMapResolver(import_map.import_map)),
          module_analyzer: &analyzer,
          ..Default::default()
        },
      )
      .await;
    graph.valid().unwrap();
    let mut eszip = super::EszipV2::from_graph(
      graph,
      &analyzer.as_capturing_parser(),
      TranspileOptions::default(),
      EmitOptions::default(),
    )
    .unwrap();
    eszip.add_import_map(ModuleKind::Jsonc, specifier.to_string(), content);

    assert_eq!(
      eszip.specifiers(),
      vec![
        "file:///deno.jsonc".to_string(),
        "file:///main.ts".to_string(),
        "file:///a.ts".to_string(),
      ],
    );

    // JSONC can be obtained by calling `get_import_map`
    let deno_jsonc = eszip.get_import_map("file:///deno.jsonc").unwrap();
    let source = deno_jsonc.source().await.unwrap();
    assert_matches_file!(
      source,
      "./testdata/deno_jsonc_as_import_map/deno.jsonc"
    );

    // JSONC can NOT be obtained as a module
    assert!(eszip.get_module("file:///deno.jsonc").is_none());
  }

  #[tokio::test]
  async fn eszipv2_iterator_yields_all_modules() {
    let loader = FileLoader {
      base_dir: "./src/testdata/deno_jsonc_as_import_map".to_string(),
    };
    let resp = deno_graph::source::Loader::load(
      &loader,
      &Url::parse("file:///deno.jsonc").unwrap(),
      LoadOptions {
        is_dynamic: false,
        cache_setting: CacheSetting::Use,
        maybe_checksum: None,
      },
    )
    .await
    .unwrap()
    .unwrap();
    let (specifier, content) = match resp {
      deno_graph::source::LoadResponse::Module {
        specifier, content, ..
      } => (specifier, content),
      _ => unimplemented!(),
    };
    let import_map = import_map::parse_from_value(
      specifier.clone(),
      jsonc_parser::parse_to_serde_value(
        &String::from_utf8(content.to_vec()).unwrap(),
        &Default::default(),
      )
      .unwrap()
      .unwrap(),
    )
    .unwrap();
    let roots = vec![ModuleSpecifier::parse("file:///main.ts").unwrap()];
    let analyzer = CapturingModuleAnalyzer::default();
    let mut graph = ModuleGraph::new(GraphKind::CodeOnly);
    graph
      .build(
        roots,
        &loader,
        BuildOptions {
          resolver: Some(&ImportMapResolver(import_map.import_map)),
          module_analyzer: &analyzer,
          ..Default::default()
        },
      )
      .await;
    graph.valid().unwrap();
    let mut eszip = super::EszipV2::from_graph(
      graph,
      &analyzer.as_capturing_parser(),
      TranspileOptions::default(),
      EmitOptions::default(),
    )
    .unwrap();
    eszip.add_import_map(ModuleKind::Jsonc, specifier.to_string(), content);

    struct Expected {
      specifier: String,
      source: &'static str,
      kind: ModuleKind,
    }

    let expected = vec![
      Expected {
        specifier: "file:///deno.jsonc".to_string(),
        source: include_str!("testdata/deno_jsonc_as_import_map/deno.jsonc"),
        kind: ModuleKind::Jsonc,
      },
      Expected {
        specifier: "file:///main.ts".to_string(),
        source: include_str!("testdata/deno_jsonc_as_import_map/main.ts"),
        kind: ModuleKind::JavaScript,
      },
      Expected {
        specifier: "file:///a.ts".to_string(),
        source: include_str!("testdata/deno_jsonc_as_import_map/a.ts"),
        kind: ModuleKind::JavaScript,
      },
    ];

    for (got, expected) in eszip.into_iter().zip(expected) {
      let (got_specifier, got_module) = got;

      assert_eq!(got_specifier, expected.specifier);
      assert_eq!(got_module.kind, expected.kind);
      assert_eq!(
        String::from_utf8_lossy(&got_module.source().await.unwrap()),
        expected.source
      );
    }
  }

  #[tokio::test]
  async fn npm_packages() {
    let roots = vec![ModuleSpecifier::parse("file:///main.ts").unwrap()];
    let analyzer = CapturingModuleAnalyzer::default();
    let mut graph = ModuleGraph::new(GraphKind::CodeOnly);
    let loader = FileLoader {
      base_dir: "./src/testdata/source".to_string(),
    };
    graph
      .build(
        roots,
        &loader,
        BuildOptions {
          module_analyzer: &analyzer,
          ..Default::default()
        },
      )
      .await;
    graph.valid().unwrap();
    let original_snapshot = SerializedNpmResolutionSnapshot {
      root_packages: root_pkgs(&[
        ("package@^1.2", "package@1.2.2"),
        ("package@^1", "package@1.2.2"),
        ("d@5", "d@5.0.0"),
      ]),
      packages: Vec::from([
        new_package("package@1.2.2", &[("a", "a@2.2.3"), ("b", "b@1.2.3")]),
        new_package("a@2.2.3", &[]),
        new_package("b@1.2.3", &[("someotherspecifier", "c@1.1.1")]),
        new_package("c@1.1.1", &[]),
        new_package("d@5.0.0", &[("e", "e@6.0.0")]),
        new_package("e@6.0.0", &[("d", "d@5.0.0")]),
      ]),
    }
    .into_valid()
    .unwrap();
    let mut eszip = super::EszipV2::from_graph(
      graph,
      &analyzer.as_capturing_parser(),
      TranspileOptions::default(),
      EmitOptions::default(),
    )
    .unwrap();
    eszip.add_npm_snapshot(original_snapshot.clone());
    let taken_snapshot = eszip.take_npm_snapshot();
    assert!(taken_snapshot.is_some());
    assert!(eszip.take_npm_snapshot().is_none());
    eszip.add_npm_snapshot(taken_snapshot.unwrap());
    let bytes = eszip.into_bytes();
    insta::assert_debug_snapshot!(bytes);
    let cursor = Cursor::new(bytes);
    let (mut eszip, fut) =
      super::EszipV2::parse(BufReader::new(AllowStdIo::new(cursor)))
        .await
        .unwrap();
    let snapshot = eszip.take_npm_snapshot().unwrap();
    assert_eq!(snapshot.as_serialized(), original_snapshot.as_serialized());

    // ensure the eszip still works otherwise
    fut.await.unwrap();
    let module = eszip.get_module("file:///main.ts").unwrap();
    assert_eq!(module.specifier, "file:///main.ts");
    let source = module.source().await.unwrap();
    assert_matches_file!(source, "./testdata/emit/main.ts");
    let source_map = module.source_map().await.unwrap();
    assert_matches_file!(source_map, "./testdata/emit/main.ts.map");
    assert_eq!(module.kind, ModuleKind::JavaScript);
    let module = eszip.get_module("file:///a.ts").unwrap();
    assert_eq!(module.specifier, "file:///b.ts");
    let source = module.source().await.unwrap();
    assert_matches_file!(source, "./testdata/emit/b.ts");
    let source_map = module.source_map().await.unwrap();
    assert_matches_file!(source_map, "./testdata/emit/b.ts.map");
    assert_eq!(module.kind, ModuleKind::JavaScript);
  }

  #[tokio::test]
  async fn npm_packages_loaded_file() {
    // packages
    let file =
      std::fs::File::open("./src/testdata/npm_packages.eszip2_1").unwrap();
    let (mut eszip, _) =
      super::EszipV2::parse(BufReader::new(AllowStdIo::new(file)))
        .await
        .unwrap();
    let npm_packages = eszip.take_npm_snapshot().unwrap();
    let expected_snapshot = SerializedNpmResolutionSnapshot {
      root_packages: root_pkgs(&[
        ("package@^1.2", "package@1.2.2"),
        ("package@^1", "package@1.2.2"),
        ("d@5", "d@5.0.0"),
      ]),
      packages: Vec::from([
        new_package("package@1.2.2", &[("a", "a@2.2.3"), ("b", "b@1.2.3")]),
        new_package("a@2.2.3", &[("b", "b@1.2.3")]),
        new_package(
          "b@1.2.3",
          &[("someotherspecifier", "c@1.1.1"), ("a", "a@2.2.3")],
        ),
        new_package("c@1.1.1", &[]),
        new_package("d@5.0.0", &[]),
      ]),
    }
    .into_valid()
    .unwrap();
    assert_eq!(
      npm_packages.as_serialized(),
      expected_snapshot.as_serialized()
    );

    // no packages
    let file =
      std::fs::File::open("./src/testdata/no_npm_packages.eszip2_1").unwrap();
    let (mut eszip, _) =
      super::EszipV2::parse(BufReader::new(AllowStdIo::new(file)))
        .await
        .unwrap();
    assert!(eszip.take_npm_snapshot().is_none());

    // invalid file with one byte changed in the npm snapshot
    let file =
      std::fs::File::open("./src/testdata/npm_packages_invalid_1.eszip2_1")
        .unwrap();
    let err = super::EszipV2::parse(BufReader::new(AllowStdIo::new(file)))
      .await
      .err()
      .unwrap();
    assert_eq!(err.to_string(), "invalid eszip v2.1 npm snapshot hash");
  }

  #[tokio::test]
  async fn npm_empty_snapshot() {
    let roots = vec![ModuleSpecifier::parse("file:///main.ts").unwrap()];
    let analyzer = CapturingModuleAnalyzer::default();
    let mut graph = ModuleGraph::new(GraphKind::CodeOnly);
    let loader = FileLoader {
      base_dir: "./src/testdata/source".to_string(),
    };
    graph
      .build(
        roots,
        &loader,
        BuildOptions {
          module_analyzer: &analyzer,
          ..Default::default()
        },
      )
      .await;
    graph.valid().unwrap();
    let original_snapshot = SerializedNpmResolutionSnapshot {
      root_packages: root_pkgs(&[]),
      packages: Vec::from([]),
    }
    .into_valid()
    .unwrap();
    let mut eszip = super::EszipV2::from_graph(
      graph,
      &analyzer.as_capturing_parser(),
      TranspileOptions::default(),
      EmitOptions::default(),
    )
    .unwrap();
    eszip.add_npm_snapshot(original_snapshot.clone());
    let bytes = eszip.into_bytes();
    insta::assert_debug_snapshot!(bytes);
    let cursor = Cursor::new(bytes);
    let (mut eszip, _) =
      super::EszipV2::parse(BufReader::new(AllowStdIo::new(cursor)))
        .await
        .unwrap();
    assert!(eszip.take_npm_snapshot().is_none());
  }

  #[tokio::test]
  async fn opaque_data() {
    let mut eszip = super::EszipV2::default();
    let opaque_data: Arc<[u8]> = Arc::new([1, 2, 3]);
    eszip.add_opaque_data("+s/foobar".to_string(), opaque_data.clone());
    let bytes = eszip.into_bytes();
    insta::assert_debug_snapshot!(bytes);
    let cursor = Cursor::new(bytes);
    let (eszip, fut) =
      super::EszipV2::parse(BufReader::new(AllowStdIo::new(cursor)))
        .await
        .unwrap();
    fut.await.unwrap();
    let opaque_data = eszip.get_module("+s/foobar").unwrap();
    assert_eq!(opaque_data.specifier, "+s/foobar");
    let source = opaque_data.source().await.unwrap();
    assert_eq!(&*source, &[1, 2, 3]);
    assert_eq!(opaque_data.kind, ModuleKind::OpaqueData);
  }

  #[tokio::test]
  async fn v2_2_backwards_compat_defaults() {
    let file = std::fs::File::open("./src/testdata/json.eszip2").unwrap();
    let (eszip, fut) =
      super::EszipV2::parse(BufReader::new(AllowStdIo::new(file)))
        .await
        .unwrap();
    fut.await.unwrap();
    assert_eq!(
      eszip.options.source_hash_function,
      Some(super::HashFunction::Sha256)
    );
    assert_eq!(eszip.options.source_digest_size(), Some(32));
  }

  #[tokio::test]
  async fn v2_2_crc32_source_checksum() {
    let mut eszip = main_eszip().await;
    eszip.set_source_hash_function(super::HashFunction::Crc32);
    let main_source = eszip
      .get_module("file:///main.ts")
      .unwrap()
      .source()
      .await
      .unwrap();
    let bytes = eszip.into_bytes();
    let main_crc32 = crc32fast::hash(&main_source).to_be_bytes();
    let crc32_in_bytes = bytes
      .windows(main_crc32.len())
      .any(|window| window == main_crc32);
    assert!(crc32_in_bytes);
    let (parsed_eszip, fut) =
      EszipV2::parse(BufReader::new(AllowStdIo::new(bytes.as_slice())))
        .await
        .unwrap();
    fut.await.unwrap();
    assert_eq!(
      parsed_eszip.options.source_hash_function,
      Some(super::HashFunction::Crc32)
    );
  }

  #[tokio::test]
  async fn v2_2_all_options_are_optional() {
    let empty_options = [
      0_u32.to_be_bytes().as_slice(),
      &<sha2::Sha256 as sha2::Digest>::digest(Vec::new()).as_slice(),
    ]
    .concat();
    let bytes = main_eszip().await.into_bytes();
    let existing_options_size = std::mem::size_of::<u32>()
      + std::mem::size_of::<u8>() * 4
      + <sha2::Sha256 as sha2::Digest>::output_size();
    let options_start = super::ESZIP_V2_2_MAGIC.len();
    let new_bytes = [
      &bytes[..options_start],
      empty_options.as_slice(),
      &bytes[options_start + existing_options_size..],
    ]
    .concat();
    let (new_eszip, fut) =
      EszipV2::parse(BufReader::new(AllowStdIo::new(new_bytes.as_slice())))
        .await
        .unwrap();
    fut.await.unwrap();

    assert_eq!(
      new_eszip.options.source_hash_function,
      Some(super::HashFunction::Sha256)
    );
    assert!(new_eszip.is_source_checksumed());
  }

  #[tokio::test]
  async fn v2_2_unknown_source_hash_function_degrades_to_no_checksum() {
    // source_hash_function 255; source_digest_size 32
    // modules are originally checksumed with sha256 (32Bytes).
    let option_bytes = &[0, 255, 1, 32];
    let futuristic_options = [
      4_u32.to_be_bytes().as_slice(),
      option_bytes,
      &<sha2::Sha256 as sha2::Digest>::digest(option_bytes).as_slice(),
    ]
    .concat();
    let bytes = main_eszip().await.into_bytes();
    let existing_options_size = std::mem::size_of::<u32>()
      + std::mem::size_of::<u8>() * 4
      + <sha2::Sha256 as sha2::Digest>::output_size();
    let options_start = super::ESZIP_V2_2_MAGIC.len();
    let new_bytes = [
      &bytes[..options_start],
      futuristic_options.as_slice(),
      &bytes[options_start + existing_options_size..],
    ]
    .concat();
    let (new_eszip, fut) =
      EszipV2::parse(BufReader::new(AllowStdIo::new(new_bytes.as_slice())))
        .await
        .unwrap();
    fut.await.unwrap();

    assert_eq!(new_eszip.options.source_hash_function, None);
    assert_eq!(new_eszip.options.source_digest_size(), Some(32));
    assert!(!new_eszip.is_source_checksumed());
  }

  #[tokio::test]
  async fn v2_2_options_forward_compatibility() {
    // source_hash_function 255; source_digest_size 32
    // modules are originally checksumed with sha256 (32Bytes).
    let option_bytes = &[255; 98];
    let futuristic_options = [
      98_u32.to_be_bytes().as_slice(),
      option_bytes,
      &<sha2::Sha256 as sha2::Digest>::digest(option_bytes).as_slice(),
    ]
    .concat();
    let bytes = main_eszip().await.into_bytes();
    let existing_options_size = std::mem::size_of::<u32>()
      + std::mem::size_of::<u8>() * 4
      + <sha2::Sha256 as sha2::Digest>::output_size();
    let options_start = super::ESZIP_V2_2_MAGIC.len();
    let new_bytes = [
      &bytes[..options_start],
      futuristic_options.as_slice(),
      &bytes[options_start + existing_options_size..],
    ]
    .concat();
    let (new_eszip, fut) =
      EszipV2::parse(BufReader::new(AllowStdIo::new(new_bytes.as_slice())))
        .await
        .unwrap();
    fut.await.unwrap();

    assert_eq!(
      new_eszip.options.source_hash_function,
      Some(super::HashFunction::Sha256)
    );
    assert_eq!(new_eszip.options.source_digest_size(), Some(32));
    assert!(new_eszip.is_source_checksumed());
  }

  fn root_pkgs(pkgs: &[(&str, &str)]) -> HashMap<PackageReq, NpmPackageId> {
    pkgs
      .iter()
      .map(|(key, value)| {
        (
          PackageReq::from_str(key).unwrap(),
          NpmPackageId::from_serialized(value).unwrap(),
        )
      })
      .collect()
  }

  fn new_package(
    id: &str,
    deps: &[(&str, &str)],
  ) -> SerializedNpmResolutionSnapshotPackage {
    SerializedNpmResolutionSnapshotPackage {
      id: NpmPackageId::from_serialized(id).unwrap(),
      dependencies: deps
        .iter()
        .map(|(key, value)| {
          (
            key.to_string(),
            NpmPackageId::from_serialized(value).unwrap(),
          )
        })
        .collect(),
      system: Default::default(),
      dist: Default::default(),
      optional_dependencies: Default::default(),
      bin: None,
    }
  }

  async fn main_eszip() -> EszipV2 {
    let roots = vec![ModuleSpecifier::parse("file:///main.ts").unwrap()];
    let analyzer = CapturingModuleAnalyzer::default();
    let mut graph = ModuleGraph::new(GraphKind::CodeOnly);
    let loader = FileLoader {
      base_dir: "./src/testdata/source".to_string(),
    };
    graph
      .build(
        roots,
        &loader,
        BuildOptions {
          module_analyzer: &analyzer,
          ..Default::default()
        },
      )
      .await;
    graph.valid().unwrap();
    super::EszipV2::from_graph(
      graph,
      &analyzer.as_capturing_parser(),
      TranspileOptions::default(),
      EmitOptions::default(),
    )
    .unwrap()
  }
}
