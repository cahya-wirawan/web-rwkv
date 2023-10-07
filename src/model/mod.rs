use anyhow::Result;
use bitflags::bitflags;
use regex::Regex;

use crate::{context::Context, tensor::TensorError};

pub mod matrix;
pub mod v4;
pub mod v5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelVersion {
    V4,
    V5,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelError {
    BatchSize(usize, usize),
    BatchOutOfRange { batch: usize, max: usize },
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelError::BatchSize(lhs, rhs) => {
                write!(f, "input batch size {lhs} not match {rhs}")
            }
            ModelError::BatchOutOfRange { batch, max } => {
                write!(f, "batch {batch} out of range of max {max}")
            }
        }
    }
}

impl std::error::Error for ModelError {}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelInfo {
    pub version: ModelVersion,
    pub num_layers: usize,
    pub num_emb: usize,
    pub num_hidden: usize,
    pub num_vocab: usize,
    pub head_size: usize,
}

pub trait BackedStateTrait: Sized {
    fn from_builder(builder: StateBuilder) -> Self;
    fn max_batch(&self) -> usize;
}

pub trait ModelStateTrait: Sized {
    type BackedState: BackedStateTrait;

    fn from_builder(builder: StateBuilder) -> Self;

    fn max_batch(&self) -> usize;

    /// Load the state from host. Their shapes must match.
    fn load(&self, backed: &Self::BackedState) -> Result<()>;
    /// Load one batch from host. The shape of the backed state should be of one batch.
    fn load_batch(&self, backed: &Self::BackedState, batch: usize) -> Result<()>;
    /// Back the entire device state to host.
    fn back(&self) -> Self::BackedState;
    /// Back one batch of the device state to host.
    fn back_batch(&self, batch: usize) -> Result<Self::BackedState>;
    /// Copy one device state to another. Their shapes must match.
    fn blit(&self, other: &Self) -> Result<(), TensorError>;
    /// Copy one batch from the source state to another.
    fn blit_batch(
        &self,
        other: &Self,
        from_batch: usize,
        to_batch: usize,
    ) -> Result<(), TensorError>;
}

pub trait ModelTrait: Sized {
    type ModelState: ModelStateTrait;

    fn from_builder(builder: ModelBuilder<'_>) -> Result<Self>;

    fn info(&self) -> &ModelInfo;

    /// Softmax of the input tensors.
    fn softmax(&self, input: Vec<Option<Vec<f32>>>) -> Result<Vec<Option<Vec<f32>>>>;

    /// Run the model for a batch of tokens as input.
    /// The length of `tokens` must match the number of batches in `state`.
    /// `tokens` may have slots with no tokens, for which `run` won't compute that batch and will return an empty vector in that corresponding slot.
    fn run(
        &self,
        tokens: &mut Vec<Vec<u16>>,
        state: &Self::ModelState,
    ) -> Result<Vec<Option<Vec<f32>>>>;
}

bitflags! {
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct LayerFlags: u64 {}
}

impl LayerFlags {
    pub fn from_layer(layer: u64) -> LayerFlags {
        LayerFlags::from_bits_retain(1 << layer)
    }

    pub fn contains_layer(&self, layer: u64) -> bool {
        self.contains(LayerFlags::from_layer(layer))
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub enum Quantization {
    /// No quantization.
    #[default]
    None,
    /// Use int8 quantization, given layers to be quantized.
    Int8(LayerFlags),
}

pub struct Lora<'a> {
    pub data: &'a [u8],
    pub blend: LoraBlend,
}

#[derive(Debug, Clone)]
pub enum LoraBlend {
    Full(f32),
    Patterns(Vec<LoraBlendPattern>),
}

impl LoraBlend {
    fn into_patterns(self) -> Vec<LoraBlendPattern> {
        match self {
            LoraBlend::Full(alpha) => {
                vec![
                    LoraBlendPattern::new(r"blocks\.[0-9]+\.([0-9a-zA-Z\.\_]+)", alpha)
                        .expect("default blend pattern"),
                ]
            }
            LoraBlend::Patterns(patterns) => patterns,
        }
    }
}

impl Default for LoraBlend {
    fn default() -> Self {
        Self::Full(1.0)
    }
}

#[derive(Debug, Clone)]
pub struct LoraBlendPattern {
    /// A regex pattern that matches tensors in the model.
    pattern: Regex,
    /// The blend factor.
    alpha: f32,
}

impl LoraBlendPattern {
    #[inline]
    pub fn new(pattern: &str, alpha: f32) -> Result<Self> {
        Ok(Self {
            pattern: Regex::new(pattern)?,
            alpha,
        })
    }

    #[inline]
    pub fn alpha(&self) -> f32 {
        self.alpha
    }
}

pub struct ModelBuilder<'a> {
    context: Context,
    data: &'a [u8],
    lora: Vec<Lora<'a>>,
    quant: Quantization,
    head_chunk_size: usize,
    token_chunk_size: usize,
}

impl<'a> ModelBuilder<'a> {
    pub fn new(context: &Context, data: &'a [u8]) -> Self {
        Self {
            context: context.clone(),
            data,
            lora: vec![],
            quant: Quantization::None,
            head_chunk_size: 4096,
            token_chunk_size: 32,
        }
    }

    pub fn with_quant(self, quant: Quantization) -> Self {
        Self { quant, ..self }
    }

    pub fn add_lora(mut self, lora: Lora<'a>) -> Self {
        self.lora.push(lora);
        self
    }

    pub fn with_head_chunk_size(self, value: usize) -> Self {
        Self {
            head_chunk_size: value,
            ..self
        }
    }

    pub fn with_token_chunk_size(self, value: usize) -> Self {
        Self {
            token_chunk_size: value,
            ..self
        }
    }

    pub fn build<M, S>(self) -> Result<M>
    where
        S: ModelStateTrait,
        M: ModelTrait<ModelState = S>,
    {
        M::from_builder(self)
    }
}

/// Create a model state.
/// - `max_batch`: The maximum number of runtime slots.
/// - `chunk_size`: Internally, the state is split into chunks of layers, since there is a size limit on one GPU buffer (128 MB).
/// If there is only one batch, it is recommended to set `chunk_size` to `info.num_layers()`.
pub struct StateBuilder {
    context: Context,
    info: ModelInfo,
    max_batch: usize,
    chunk_size: usize,
}

impl StateBuilder {
    pub fn new(context: &Context, info: &ModelInfo) -> Self {
        Self {
            context: context.clone(),
            info: info.clone(),
            max_batch: 1,
            chunk_size: info.num_layers,
        }
    }

    pub fn with_max_batch(self, value: usize) -> Self {
        Self {
            max_batch: value,
            ..self
        }
    }

    pub fn with_chunk_size(self, value: usize) -> Self {
        Self {
            chunk_size: value,
            ..self
        }
    }

    pub fn build<S: ModelStateTrait>(self) -> S {
        S::from_builder(self)
    }

    pub fn build_backed<B: BackedStateTrait>(self) -> B {
        B::from_builder(self)
    }
}