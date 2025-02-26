use std::{
    collections::HashMap,
    fmt::{Display, Formatter},
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use crate::{
    util, Hyperparameters, KnownModel, LoraAdapter, LoraParameters, ModelParameters, TokenId,
    Vocabulary,
};
pub use ggml::ContainerType;
use ggml::{
    format::{LoadError as FormatLoadError, PartialHyperparameters, TensorLoadInfo},
    Context,
};
use memmap2::Mmap;
use thiserror::Error;

#[derive(Debug, PartialEq, Clone, Copy, Eq, Default)]
/// Information about the file.
pub struct FileType {
    /// The format of the tensors.
    pub format: FileTypeFormat,
    /// The quantization version.
    pub quantization_version: u32,
}
impl From<FileType> for i32 {
    fn from(value: FileType) -> Self {
        (value.quantization_version * ggml::QNT_VERSION_FACTOR) as i32
            + match value.format {
                FileTypeFormat::F32 => 0,
                FileTypeFormat::MostlyF16 => 1,
                FileTypeFormat::MostlyQ4_0 => 2,
                FileTypeFormat::MostlyQ4_1 => 3,
                FileTypeFormat::MostlyQ4_1SomeF16 => 4,
                FileTypeFormat::MostlyQ4_2 => 5,
                FileTypeFormat::MostlyQ8_0 => 7,
                FileTypeFormat::MostlyQ5_0 => 8,
                FileTypeFormat::MostlyQ5_1 => 9,
            }
    }
}
impl TryFrom<i32> for FileType {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        let format = match (value as u32) % ggml::QNT_VERSION_FACTOR {
            0 => FileTypeFormat::F32,
            1 => FileTypeFormat::MostlyF16,
            2 => FileTypeFormat::MostlyQ4_0,
            3 => FileTypeFormat::MostlyQ4_1,
            4 => FileTypeFormat::MostlyQ4_1SomeF16,
            5 => FileTypeFormat::MostlyQ4_2,
            7 => FileTypeFormat::MostlyQ8_0,
            8 => FileTypeFormat::MostlyQ5_0,
            9 => FileTypeFormat::MostlyQ5_1,
            _ => return Err(()),
        };

        Ok(Self {
            format,
            quantization_version: (value as u32) / ggml::QNT_VERSION_FACTOR,
        })
    }
}
impl Display for FileType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.format {
            FileTypeFormat::F32 => write!(f, "f32"),
            FileTypeFormat::MostlyF16 => write!(f, "f16"),
            FileTypeFormat::MostlyQ4_0 => write!(f, "q4_0"),
            FileTypeFormat::MostlyQ4_1 => write!(f, "q4_1"),
            FileTypeFormat::MostlyQ4_1SomeF16 => write!(f, "q4_1_with_f16"),
            FileTypeFormat::MostlyQ4_2 => write!(f, "q4_2"),
            FileTypeFormat::MostlyQ8_0 => write!(f, "q8_0"),
            FileTypeFormat::MostlyQ5_0 => write!(f, "q5_0"),
            FileTypeFormat::MostlyQ5_1 => write!(f, "q5_1"),
        }?;

        write!(f, "_qnt{}", self.quantization_version)?;

        Ok(())
    }
}

/// How the tensors are stored in GGML LLM models.
#[derive(Debug, PartialEq, Clone, Copy, Eq, Default)]
pub enum FileTypeFormat {
    /// All tensors are stored as f32.
    F32,
    #[default]
    /// All tensors are mostly stored as `f16`, except for the 1D tensors (32-bit).
    MostlyF16,
    /// All tensors are mostly stored as `Q4_0`, except for the 1D tensors (32-bit).
    MostlyQ4_0,
    /// All tensors are mostly stored as `Q4_1`, except for the 1D tensors (32-bit)
    MostlyQ4_1,
    /// All tensors are mostly stored as `Q4_1`, except for the 1D tensors (32-bit)
    /// and the `tok_embeddings.weight` (f16) and `output.weight` tensors (f16).
    MostlyQ4_1SomeF16,
    /// All tensors are mostly stored as `Q4_2`, except for the 1D tensors (32-bit).
    MostlyQ4_2,
    /// All tensors are mostly stored as `Q8_0`, except for the 1D tensors (32-bit).
    MostlyQ8_0,
    /// All tensors are mostly stored as `Q5_0`, except for the 1D tensors (32-bit).
    MostlyQ5_0,
    /// All tensors are mostly stored as `Q5_1`, except for the 1D tensors (32-bit).
    MostlyQ5_1,
}
impl TryFrom<ggml::Type> for FileTypeFormat {
    type Error = ();

    fn try_from(value: ggml::Type) -> Result<Self, Self::Error> {
        Ok(match value {
            ggml::Type::Q4_0 => Self::MostlyQ4_0,
            ggml::Type::Q4_1 => Self::MostlyQ4_1,
            ggml::Type::Q5_0 => Self::MostlyQ5_0,
            ggml::Type::Q5_1 => Self::MostlyQ5_1,
            ggml::Type::Q8_0 => Self::MostlyQ8_0,
            ggml::Type::Q8_1 => return Err(()),
            ggml::Type::I32 => return Err(()),
            ggml::Type::F16 => Self::MostlyF16,
            ggml::Type::F32 => Self::F32,
            ggml::Type::LegacyQ4_2 => Self::MostlyQ4_2,
        })
    }
}

/// Each variant represents a step within the process of loading the model.
/// These can be used to report progress to the user.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LoadProgress {
    /// The hyperparameters have been loaded from the model.
    HyperparametersLoaded,
    /// The context has been created.
    ContextSize {
        /// The size of the context.
        bytes: usize,
    },
    /// A tensor was patched with a LoRA.
    LoraApplied {
        /// The name of the patched tensor.
        name: String,
        /// LoRA file the patch was applied from.
        source: PathBuf,
    },
    /// A tensor from the current part has been loaded.
    TensorLoaded {
        /// The current tensor (0-indexed).
        current_tensor: usize,
        /// The number of total tensors.
        tensor_count: usize,
    },
    /// A model part has finished fully loading.
    Loaded {
        /// The number of bytes in the part.
        file_size: u64,
        /// The number of tensors in the part.
        tensor_count: usize,
    },
}

#[derive(Error, Debug)]
/// Errors encountered during the loading process.
pub enum LoadError {
    #[error("the file {path:?} does not exist")]
    /// The file does not exist.
    FileDoesNotExist {
        /// The path that failed.
        path: PathBuf,
    },
    #[error("could not open file {path:?}")]
    /// A file failed to open.
    OpenFileFailed {
        /// The original error.
        source: std::io::Error,
        /// The path that failed.
        path: PathBuf,
    },
    #[error("no parent path for {path:?}")]
    /// There is no parent path for a given path.
    NoParentPath {
        /// The path without a parent.
        path: PathBuf,
    },
    #[error("unable to read exactly {bytes} bytes")]
    /// Reading exactly `bytes` from a file failed.
    ReadExactFailed {
        /// The original error.
        source: std::io::Error,
        /// The number of bytes that were attempted to be read.
        bytes: usize,
    },
    #[error("non-specific I/O error")]
    /// A non-specific IO error.
    Io(#[from] std::io::Error),
    #[error("could not convert bytes to a UTF-8 string")]
    /// One of the strings encountered was not valid UTF-8.
    InvalidUtf8(#[from] std::string::FromUtf8Error),
    #[error("invalid integer conversion")]
    /// One of the integers encountered could not be converted to a more appropriate type.
    InvalidIntegerConversion(#[from] std::num::TryFromIntError),
    #[error("unsupported f16_: {0}")]
    /// The `f16_` hyperparameter had an invalid value.
    UnsupportedFileType(i32),
    #[error("invalid magic number {magic:#x} for {path:?}")]
    /// An invalid magic number was encountered during the loading process.
    InvalidMagic {
        /// The path that failed.
        path: PathBuf,
        /// The magic number that was encountered.
        magic: u32,
    },
    #[error("invalid file format {container_type:?}")]
    /// The version of the format is not supported by this version of `llm`.
    InvalidFormatVersion {
        /// The format that was encountered.
        container_type: ContainerType,
    },
    #[error("invalid value {ftype} for `f16` in hyperparameters")]
    /// The `f16` hyperparameter had an invalid value.
    HyperparametersF16Invalid {
        /// The format type that was encountered.
        ftype: i32,
    },
    #[error("unknown tensor `{tensor_name}` in {path:?}")]
    /// The tensor `tensor_name` was encountered during the loading of `path`, but was not seen during
    /// the model prelude.
    UnknownTensor {
        /// The name of the tensor.
        tensor_name: String,
        /// The path that failed.
        path: PathBuf,
    },
    #[error("the tensor `{tensor_name}` has the wrong size in {path:?}")]
    /// The tensor `tensor_name` did not match its expected size.
    TensorWrongSize {
        /// The name of the tensor.
        tensor_name: String,
        /// The path that failed.
        path: PathBuf,
    },
    /// The tensor `tensor_name` did not have the expected format type.
    #[error("invalid ftype {ftype} for tensor `{tensor_name}` in {path:?}")]
    UnsupportedElementType {
        /// The name of the tensor.
        tensor_name: String,
        /// The format type that was encountered.
        ftype: u32,
        /// The path that failed.
        path: PathBuf,
    },
    /// An invariant was broken.
    ///
    /// This error is not relevant unless `loader2` is being used.
    #[error("invariant broken: {invariant} in {path:?}")]
    InvariantBroken {
        /// The path that failed.
        path: Option<PathBuf>,
        /// The invariant that was broken.
        invariant: String,
    },
    /// The model could not be created.
    ///
    /// This implies that there were no tensors in the model to be loaded.
    ///
    /// This error is not relevant unless `loader2` is being used.
    #[error("could not create model from {path:?}")]
    ModelNotCreated {
        /// The path that failed.
        path: PathBuf,
    },
    /// Multiple parts of the model were found.
    ///
    /// Multi-part models are not supported. Please convert the model to a single part.
    #[error("multipart models are not supported")]
    MultipartNotSupported {
        /// The paths that were found.
        paths: Vec<PathBuf>,
    },
}
impl From<util::FindAllModelFilesError> for LoadError {
    fn from(value: util::FindAllModelFilesError) -> Self {
        match value {
            util::FindAllModelFilesError::NoParentPath { path } => LoadError::NoParentPath { path },
            util::FindAllModelFilesError::IO(err) => LoadError::Io(err),
        }
    }
}

impl LoadError {
    #[doc(hidden)]
    pub fn from_format_error(value: FormatLoadError<LoadError>, path: PathBuf) -> Self {
        match value {
            FormatLoadError::InvalidMagic(magic) => LoadError::InvalidMagic { path, magic },
            FormatLoadError::InvalidFormatVersion(container_type) => {
                LoadError::InvalidFormatVersion { container_type }
            }
            FormatLoadError::Io(err) => LoadError::Io(err),
            FormatLoadError::InvalidUtf8(err) => LoadError::InvalidUtf8(err),
            FormatLoadError::InvalidIntegerConversion(err) => {
                LoadError::InvalidIntegerConversion(err)
            }
            FormatLoadError::ImplementationError(err) => err,
            FormatLoadError::UnsupportedElementType { tensor_name, ftype } => {
                LoadError::UnsupportedElementType {
                    path,
                    tensor_name,
                    ftype,
                }
            }
            FormatLoadError::InvariantBroken(invariant) => LoadError::InvariantBroken {
                path: Some(path),
                invariant,
            },
        }
    }
}

/// Used by models to fetch tensors from a loader.
pub trait TensorLoader<E: std::error::Error> {
    /// Gets a tensor from the loader.
    fn load(&mut self, name: &str) -> Result<ggml::Tensor, E>;
    /// Finish loading the model, and extract all of the state from the loader.
    fn finish(self) -> (Context, HashMap<String, ggml::Tensor>, Option<Mmap>);
}

/// Load a GGML model from the `path` and configure it per the `params`. The status
/// of the loading process will be reported through `load_progress_callback`.
///
/// Note that the model must be a single-part model, and the model in `path`
/// *must* match the architecture of `M`.
///
/// # Panics
///
/// - If the model does not match the architecture of `M`. This is not checked
///   before execution, so this function will panic if the model does not match
///   the architecture.
///
///   This is a limitation of the GGML format, which does not
///   store any information about the architecture.
pub fn load<M: KnownModel>(
    path: &Path,
    params: ModelParameters,
    overrides: Option<M::Overrides>,
    load_progress_callback: impl FnMut(LoadProgress),
) -> Result<M, LoadError> {
    if !path.exists() {
        return Err(LoadError::FileDoesNotExist {
            path: path.to_owned(),
        });
    }

    let paths = util::find_all_model_files(path)?;
    if paths.len() != 1 {
        return Err(LoadError::MultipartNotSupported { paths });
    }

    let file = File::open(path).map_err(|e| LoadError::OpenFileFailed {
        source: e,
        path: path.to_owned(),
    })?;
    let mut reader = BufReader::new(&file);

    let mut loader = Loader::new(load_progress_callback);

    ggml::format::load(&mut reader, &mut loader)
        .map_err(|err| LoadError::from_format_error(err, path.to_owned()))?;

    let Loader {
        hyperparameters,
        vocabulary,
        tensors,
        mut load_progress_callback,
        container_type,
        ..
    } = loader;

    let quantization_version = (&hyperparameters as &M::Hyperparameters)
        .file_type()
        .map(|ft| ft.quantization_version)
        .unwrap_or_default();
    let quantization_version = if quantization_version == 0 {
        // HACK: I think llama.cpp does not actually write the quantization version correctly,
        // so we need to guess it from the container type.
        if container_type == ggml::ContainerType::Ggjt(2) {
            1
        } else if container_type == ggml::ContainerType::Ggjt(3) {
            2
        } else {
            quantization_version
        }
    } else {
        quantization_version
    };

    // TODO: this is temporary while we figure out how to handle this
    if tensors.values().any(|t| t.element_type.is_quantized()) {
        assert_eq!(quantization_version, 2, "quantization version must be 2");
    }

    let use_mmap =
        params.prefer_mmap && container_type.support_mmap() && params.lora_adapters.is_none();

    let ctx_size = tensors
        .values()
        .map(|ti| ti.calc_absolute_size(use_mmap))
        .sum::<usize>();

    let mut lora_adapters: Option<Vec<LoraAdapter>> = None;
    if let Some(lora_paths) = &params.lora_adapters {
        let adapters: Result<Vec<_>, _> = lora_paths
            .iter()
            .map(|lora_path| {
                // Read the LoRA file
                let lora_file = File::open(lora_path).map_err(|e| LoadError::OpenFileFailed {
                    source: e,
                    path: lora_path.to_owned(),
                })?;
                let mut lora_reader = BufReader::new(&lora_file);
                // TODO: Consider updating the progress callback to report the progress of the LoRA file.
                // Most LoRAs are small enough that this is not necessary, but it would be nice to have.
                let mut lora_loader: Loader<LoraParameters, _> = Loader::new(|_| {});
                ggml::format::load(&mut lora_reader, &mut lora_loader)
                    .map_err(|err| LoadError::from_format_error(err, lora_path.to_owned()))?;

                // Collect the names of the tensors that should be patched
                let tensors_to_patch = lora_loader
                    .tensors
                    .keys()
                    .filter_map(|k| Some(k.rsplit_once('.')?.0.to_owned()))
                    .collect();

                // Return the LoRA patches
                Ok::<_, LoadError>(LoraAdapter {
                    scaling: lora_loader.hyperparameters.calculate_scaling(),
                    tensors: lora_loader.tensors,
                    tensors_to_patch,
                    file: lora_file,
                    path: lora_path.to_owned(),
                })
            })
            .collect();
        lora_adapters = Some(adapters?);
    }

    (load_progress_callback)(LoadProgress::ContextSize { bytes: ctx_size });
    let context = Context::init(ctx_size, !use_mmap);

    let (mmap, file_size) = {
        let file = File::open(path)?;
        let mmap = if use_mmap {
            Some(unsafe { Mmap::map(&file)? })
        } else {
            None
        };
        (mmap, file.metadata()?.len())
    };

    let tensors_len = tensors.len();
    let tl = MmapCompatibleLoader {
        path: path.to_owned(),
        file,
        tensors,
        context,
        mmap,
        lora_adapters,
        load_progress_callback: &mut load_progress_callback,
        loaded_tensors: Default::default(),
    };

    let model = KnownModel::new(hyperparameters, params, overrides, vocabulary, tl)?;

    (load_progress_callback)(LoadProgress::Loaded {
        file_size,
        tensor_count: tensors_len,
    });

    Ok(model)
}

/// A GGML format loader for LLMs.
pub struct Loader<Hp: Hyperparameters, F: FnMut(LoadProgress)> {
    // Input
    load_progress_callback: F,

    // Output
    /// The container type of the model.
    pub container_type: ContainerType,
    /// The hyperparameters of the model.
    pub hyperparameters: Hp,
    /// The vocabulary of the model.
    pub vocabulary: Vocabulary,
    /// The tensors of the model.
    pub tensors: HashMap<String, TensorLoadInfo>,
}
impl<Hp: Hyperparameters, F: FnMut(LoadProgress)> Loader<Hp, F> {
    /// Creates a new loader.
    pub fn new(load_progress_callback: F) -> Self {
        Self {
            load_progress_callback,

            container_type: ContainerType::Ggml,
            hyperparameters: Hp::default(),
            vocabulary: Vocabulary::default(),
            tensors: HashMap::default(),
        }
    }
}
impl<Hp: Hyperparameters, F: FnMut(LoadProgress)> ggml::format::LoadHandler<LoadError>
    for Loader<Hp, F>
{
    fn container_type(&mut self, container_type: ContainerType) -> Result<(), LoadError> {
        self.container_type = container_type;
        Ok(())
    }

    fn vocabulary_token(&mut self, i: usize, token: Vec<u8>, score: f32) -> Result<(), LoadError> {
        let id = match TokenId::try_from(i) {
            Ok(id) => id,
            Err(err) => return Err(LoadError::InvalidIntegerConversion(err)),
        };
        self.vocabulary.push_token(id, token, score);

        Ok(())
    }

    fn read_hyperparameters(
        &mut self,
        reader: &mut dyn BufRead,
    ) -> Result<PartialHyperparameters, LoadError> {
        // NOTE: Field order matters! Data is laid out in the file exactly in this order.
        let hyperparameters = Hp::read_ggml(reader)?;
        let partial = PartialHyperparameters {
            n_vocab: hyperparameters.n_vocabulary(),
        };
        self.hyperparameters = hyperparameters;
        (self.load_progress_callback)(LoadProgress::HyperparametersLoaded);

        Ok(partial)
    }

    fn tensor_buffer(&mut self, info: TensorLoadInfo) -> Result<(), LoadError> {
        self.tensors.insert(info.name.clone(), info);
        Ok(())
    }
}

struct MmapCompatibleLoader<'a> {
    path: PathBuf,
    file: File,
    tensors: HashMap<String, TensorLoadInfo>,
    context: Context,
    mmap: Option<Mmap>,
    lora_adapters: Option<Vec<LoraAdapter>>,
    load_progress_callback: &'a mut dyn FnMut(LoadProgress),
    loaded_tensors: HashMap<String, ggml::Tensor>,
}
impl TensorLoader<LoadError> for MmapCompatibleLoader<'_> {
    fn load(&mut self, name: &str) -> Result<ggml::Tensor, LoadError> {
        let info = self.tensors.get(name).ok_or(LoadError::UnknownTensor {
            tensor_name: String::from(name),
            path: Default::default(),
        })?;

        let mut main_context = FileContext::new(
            &self.context,
            &mut self.file,
            &self.path,
            self.mmap.as_ref(),
        );

        let mut tensor = main_context.get_tensor(info)?;

        if let Some(lora_adapters) = &mut self.lora_adapters {
            for lora_adapter in lora_adapters {
                lora_adapter.patch(info, &mut tensor)?;
                (self.load_progress_callback)(LoadProgress::LoraApplied {
                    name: name.to_owned(),
                    source: lora_adapter.path.to_owned(),
                });
            }
        }

        (self.load_progress_callback)(LoadProgress::TensorLoaded {
            current_tensor: self.loaded_tensors.len(),
            tensor_count: self.tensors.len(),
        });
        self.loaded_tensors.insert(name.to_owned(), tensor.share());

        Ok(tensor)
    }

    fn finish(self) -> (Context, HashMap<String, ggml::Tensor>, Option<Mmap>) {
        (self.context, self.loaded_tensors, self.mmap)
    }
}

pub(crate) struct FileContext<'a> {
    context: &'a Context,
    file: &'a mut File,
    path: &'a Path,
    mmap: Option<&'a Mmap>,
}
impl<'a> FileContext<'a> {
    pub(crate) fn new(
        context: &'a Context,
        file: &'a mut File,
        path: &'a Path,
        mmap: Option<&'a Mmap>,
    ) -> Self {
        Self {
            context,
            file,
            path,
            mmap,
        }
    }

    pub(crate) fn get_tensor(&mut self, info: &TensorLoadInfo) -> Result<ggml::Tensor, LoadError> {
        let name = &info.name;
        let ne = info.dims();
        let dims = ne.len();

        if dims != info.n_dims {
            return Err(LoadError::InvariantBroken {
                path: Some(self.path.to_owned()),
                invariant: format!(
                    "the tensor {name} should have {} dimensions, not {}",
                    info.n_dims, dims
                ),
            });
        }

        let mut tensor = match dims {
            1 => self.context.new_tensor_1d(info.element_type, ne[0]),
            2 => self.context.new_tensor_2d(info.element_type, ne[0], ne[1]),
            3 => self
                .context
                .new_tensor_3d(info.element_type, ne[0], ne[1], ne[2]),
            _ => {
                return Err(LoadError::InvariantBroken {
                    path: Some(self.path.to_owned()),
                    invariant: format!(
                        "the tensor {name} should have between 1 and 3 dimensions, not {dims}"
                    ),
                })
            }
        };

        match self.mmap {
            Some(mmap) => unsafe {
                let ptr = mmap.as_ptr().offset(info.start_offset as isize);
                tensor.set_data(ptr as *mut std::ffi::c_void);
            },
            None => {
                let buf: &mut [u8] = unsafe {
                    std::slice::from_raw_parts_mut(tensor.data() as *mut u8, tensor.nbytes())
                };
                self.file.seek(SeekFrom::Start(info.start_offset))?;
                self.file.read_exact(buf)?;
            }
        }

        Ok(tensor)
    }
}

/// A implementation for `load_progress_callback` that outputs to `stdout`.
pub fn load_progress_callback_stdout(progress: LoadProgress) {
    match progress {
        LoadProgress::HyperparametersLoaded => println!("Loaded hyperparameters"),
        LoadProgress::ContextSize { bytes } => println!(
            "ggml ctx size = {:.2} MB\n",
            bytes as f64 / (1024.0 * 1024.0)
        ),
        LoadProgress::TensorLoaded {
            current_tensor,
            tensor_count,
            ..
        } => {
            let current_tensor = current_tensor + 1;
            if current_tensor % 8 == 0 {
                println!("Loaded tensor {current_tensor}/{tensor_count}");
            }
        }
        LoadProgress::Loaded {
            file_size: byte_size,
            tensor_count,
        } => {
            println!("Loading of model complete");
            println!(
                "Model size = {:.2} MB / num tensors = {}",
                byte_size as f64 / 1024.0 / 1024.0,
                tensor_count
            );
        }
        LoadProgress::LoraApplied { name, source } => {
            println!(
                "Patched tensor {} via LoRA from '{}'",
                name,
                source.file_name().unwrap().to_str().unwrap()
            );
        }
    };
}
