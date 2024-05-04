use std::{collections::HashMap, marker::PhantomData, sync::Arc};

use anyhow::Result;
use futures::future::BoxFuture;
use half::f16;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use web_rwkv_derive::DeserializeSeed;
use wgpu::CommandBuffer;

use super::{
    infer::{
        InferChunk, InferInfo, InferOutput, InferOutputBatch, InferRedirect, MIN_TOKEN_CHUNK_SIZE,
    },
    loader::{Loader, Reader},
    model::{AsAny, Build, EmbedDevice, ModelBuilder, ModelInfo, PassId, Quant, State as _},
    Job, JobBuilder,
};
use crate::{
    context::Context,
    num::Float,
    tensor::{
        kind::ReadWrite,
        matrix::Matrix,
        ops::{Activation, TensorCommand, TensorOp, TensorPass},
        shape::Shape,
        DeepClone, IntoPackedCursors, TensorCpu, TensorError, TensorGpu, TensorGpuView, TensorInit,
        TensorShape, TensorStack,
    },
};

#[derive(Debug, Clone, Serialize, DeserializeSeed)]
pub struct Model {
    pub context: Context,
    pub info: ModelInfo,
    pub tensor: ModelTensor,
}

impl Model {
    pub const RESCALE_LAYER: usize = 6;

    pub const LN_EPS: f32 = 1.0e-5;
    pub const GN_EPS: f32 = 64.0e-5;
}

#[derive(Debug, Clone, Serialize, DeserializeSeed)]
pub struct ModelTensor {
    pub embed: Embed,
    pub head: Head,
    pub layers: Vec<Layer>,
}

#[derive(Debug, Clone, Serialize, DeserializeSeed)]
pub struct LayerNorm {
    pub w: TensorGpu<f16, ReadWrite>,
    pub b: TensorGpu<f16, ReadWrite>,
}

#[derive(Debug, Clone, Serialize, DeserializeSeed)]
pub struct Att {
    pub time_decay: TensorGpu<f32, ReadWrite>,
    pub time_first: TensorGpu<f32, ReadWrite>,

    pub time_mix_k: TensorGpu<f16, ReadWrite>,
    pub time_mix_v: TensorGpu<f16, ReadWrite>,
    pub time_mix_r: TensorGpu<f16, ReadWrite>,

    pub w_k: Matrix,
    pub w_v: Matrix,
    pub w_r: Matrix,
    pub w_o: Matrix,
}

#[derive(Debug, Clone, Serialize, DeserializeSeed)]
pub struct Ffn {
    pub time_mix_k: TensorGpu<f16, ReadWrite>,
    pub time_mix_r: TensorGpu<f16, ReadWrite>,

    pub w_k: Matrix,
    pub w_v: Matrix,
    pub w_r: Matrix,
}

#[derive(Debug, Clone, Serialize, DeserializeSeed)]
pub struct Layer {
    pub att_layer_norm: LayerNorm,
    pub ffn_layer_norm: LayerNorm,
    pub att: Att,
    pub ffn: Ffn,
}

#[derive(Debug, Clone, Serialize, DeserializeSeed)]
pub struct Embed {
    pub layer_norm: LayerNorm,
    pub w: TensorCpu<f16>,
    pub u: Option<TensorGpu<f16, ReadWrite>>,
}

#[derive(Debug, Clone, Serialize, DeserializeSeed)]
pub struct Head {
    pub layer_norm: LayerNorm,
    pub w: Matrix,
}

#[derive(Debug, Clone, Serialize, DeserializeSeed)]
pub struct State {
    pub context: Context,
    pub info: ModelInfo,
    pub data: TensorGpu<f32, ReadWrite>,
}

impl State {
    async fn back(&self, batch: usize) -> Result<TensorCpu<f32>, TensorError> {
        let context = &self.context;

        let shape = self.data.shape();
        let tensor: TensorGpu<f32, ReadWrite> = context.tensor_init([shape[0], shape[1], 1, 1]);
        let mut encoder = context.device.create_command_encoder(&Default::default());
        encoder.copy_tensor_batch(&self.data, &tensor, batch, 0)?;
        context.queue.submit(Some(encoder.finish()));

        Ok(tensor.back().await)
    }
}

impl AsAny for State {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl super::model::State for State {
    #[inline]
    fn num_batch(&self) -> usize {
        self.data.shape()[2]
    }

    fn init(&self) -> TensorCpu<f32> {
        let info = &self.info;
        let data = (0..info.num_layer)
            .map(|_| {
                [
                    vec![0.0; info.num_emb],
                    vec![0.0; info.num_emb],
                    vec![0.0; info.num_emb],
                    vec![f32::MIN; info.num_emb],
                    vec![0.0; info.num_emb],
                ]
                .concat()
            })
            .collect_vec()
            .concat();
        let shape = Shape::new(info.num_emb, 5 * info.num_layer, 1, 1);
        TensorCpu::from_data(shape, data).unwrap()
    }

    fn att(&self, layer: usize) -> Result<TensorGpuView<f32>, TensorError> {
        let start = 5 * layer;
        let end = start + 4;
        self.data.view(.., start..end, .., ..)
    }

    fn ffn(&self, layer: usize) -> Result<TensorGpuView<f32>, TensorError> {
        let start = 5 * layer + 4;
        self.data.view(.., start..=start, .., ..)
    }

    fn load(&self, batch: usize, tensor: TensorCpu<f32>) -> Result<(), TensorError> {
        tensor.check_shape([self.info.num_emb, self.info.num_layer * 5, 1, 1])?;
        self.data.load_batch(&tensor, batch)?;
        Ok(())
    }

    fn back(&self, batch: usize) -> BoxFuture<Result<TensorCpu<f32>, TensorError>> {
        Box::pin(self.back(batch))
    }

    fn embed(&self, layer: usize, backed: TensorCpu<f32>) -> Result<TensorCpu<f32>, TensorError> {
        backed.slice(.., layer, .., ..)
    }
}

impl DeepClone for State {
    fn deep_clone(&self) -> Self {
        let data = self.data.deep_clone();
        Self {
            data,
            ..self.clone()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Runtime<F: Float> {
    pub tokens: TensorGpu<u32, ReadWrite>,
    pub cursors: TensorGpu<u32, ReadWrite>,
    pub input: TensorGpu<F, ReadWrite>,

    pub att_x: TensorGpu<F, ReadWrite>,
    pub att_kx: TensorGpu<F, ReadWrite>,
    pub att_vx: TensorGpu<F, ReadWrite>,
    pub att_rx: TensorGpu<F, ReadWrite>,
    pub att_k: TensorGpu<f32, ReadWrite>,
    pub att_v: TensorGpu<f32, ReadWrite>,
    pub att_r: TensorGpu<f32, ReadWrite>,
    pub att_o: TensorGpu<F, ReadWrite>,

    pub ffn_x: TensorGpu<F, ReadWrite>,
    pub ffn_kx: TensorGpu<F, ReadWrite>,
    pub ffn_rx: TensorGpu<F, ReadWrite>,
    pub ffn_k: TensorGpu<F, ReadWrite>,
    pub ffn_v: TensorGpu<F, ReadWrite>,
    pub ffn_r: TensorGpu<F, ReadWrite>,

    pub aux_x: TensorGpu<f32, ReadWrite>,
}

impl<F: Float> Runtime<F> {
    pub fn new(context: &Context, info: &ModelInfo, num_token: usize) -> Self {
        let shape = Shape::new(info.num_emb, num_token, 1, 1);
        let cursors_shape = Shape::new(num_token, 1, 1, 1);
        let tokens_shape = Shape::new(num_token, 1, 1, 1);
        let hidden_shape = Shape::new(info.num_hidden, num_token, 1, 1);

        Self {
            cursors: context.tensor_init(cursors_shape),
            tokens: context.tensor_init(tokens_shape),
            input: context.tensor_init(shape),
            att_x: context.tensor_init(shape),
            att_kx: context.tensor_init(shape),
            att_vx: context.tensor_init(shape),
            att_rx: context.tensor_init(shape),
            att_k: context.tensor_init(shape),
            att_v: context.tensor_init(shape),
            att_r: context.tensor_init(shape),
            att_o: context.tensor_init(shape),
            ffn_x: context.tensor_init(shape),
            ffn_kx: context.tensor_init(shape),
            ffn_rx: context.tensor_init(shape),
            ffn_k: context.tensor_init(hidden_shape),
            ffn_v: context.tensor_init(shape),
            ffn_r: context.tensor_init(shape),
            aux_x: context.tensor_init(shape),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Header<F: Float> {
    pub head_x: TensorGpu<F, ReadWrite>,
    pub head_o: TensorGpu<F, ReadWrite>,
}

impl<F: Float> Header<F> {
    pub fn new(context: &Context, info: &ModelInfo, num_header: usize) -> Self {
        let head_shape = Shape::new(info.num_emb, num_header, 1, 1);
        let output_shape = Shape::new(info.num_vocab, num_header, 1, 1);

        Self {
            head_x: context.tensor_init(head_shape),
            head_o: context.tensor_init(output_shape),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Hook {
    PostEmbedLoaded,
    PostEmbedLayerNorm,
    PreAtt(usize),
    PostAttLayerNorm(usize),
    PreAttTokenShift(usize),
    PostAttTokenShift(usize),
    PreAttLinear(usize),
    PostAttLinear(usize),
    PreAttTimeMix(usize),
    PostAttTimeMix(usize),
    PreAttOut(usize),
    PostAttOut(usize),
    PostAtt(usize),
    PreFfn(usize),
    PostFfnLayerNorm(usize),
    PreFfnTokenShift(usize),
    PostFfnTokenShift(usize),
    PreFfnLinear(usize),
    PostFfnLinear(usize),
    PostFfnActivate(usize),
    PreFfnChannelMix(usize),
    PostFfnChannelMix(usize),
    PostFfn(usize),
    PreHead,
    PostHeadLayerNorm,
    PostHead,
}

pub struct InferJob<F: Float> {
    commands: Vec<CommandBuffer>,
    redirect: InferRedirect,

    embed_device: EmbedDevice,
    embed: TensorCpu<f16>,

    cursors: TensorGpu<u32, ReadWrite>,
    tokens: TensorGpu<u32, ReadWrite>,
    input: TensorGpu<F, ReadWrite>,
    output: TensorGpu<F, ReadWrite>,
}

impl<F: Float> Job for InferJob<F> {
    type Info = InferInfo;
    type Input = InferChunk;
    type Output = InferOutput<F>;

    fn check(&self, input: &Self::Input, info: &Self::Info) -> bool {
        input.num_token() == self.cursors.shape()[0] && info.redirect() == self.redirect
    }

    fn load(self, input: &Self::Input) -> Result<Self> {
        if input.num_token() == 0 {
            return Ok(self);
        }

        let stack: Vec<TensorCpu<F>> = input
            .iter()
            .map(|chunk| -> Result<TensorCpu<F>, _> {
                let num_emb = self.embed.shape()[0];
                let num_token = chunk.len();
                let data = self.embed.data();
                let data = chunk
                    .iter()
                    .map(|&token| {
                        let start = num_emb * token as usize;
                        let end = start + num_emb;
                        data[start..end].to_vec()
                    })
                    .concat();
                let data = data.into_iter().map(|x| F::co_hom(x)).collect_vec();
                let shape = Shape::new(num_emb, num_token, 1, 1);
                TensorCpu::from_data(shape, data)
            })
            .try_collect()?;
        let stack = TensorStack::try_from(stack)?;

        let cursors = stack.cursors.clone().into_cursors();
        let cursors = TensorCpu::from_data(self.cursors.shape(), cursors)?;
        self.cursors.load(&cursors)?;

        match self.embed_device {
            EmbedDevice::Cpu => self.input.load(&stack.tensor)?,
            EmbedDevice::Gpu => {
                let tokens = input
                    .iter()
                    .map(|chunk| chunk.0.clone())
                    .concat()
                    .into_iter()
                    .map(|token| token as u32)
                    .collect_vec();
                let tokens = TensorCpu::from_data(self.tokens.shape(), tokens)?;
                self.tokens.load(&tokens)?;
            }
        }

        Ok(self)
    }

    fn submit(&mut self) {
        let commands = std::mem::take(&mut self.commands);
        self.output.context.queue.submit(commands);
    }

    async fn back(self) -> Result<Self::Output> {
        let output = self.output.back().await;
        let batches: Vec<_> = self
            .redirect
            .outputs
            .into_iter()
            .map(|(start, end)| output.slice(.., start..end, .., ..))
            .try_collect()?;
        let batches = batches.into_iter().map(InferOutputBatch).collect();
        Ok(InferOutput(batches))
    }
}

#[derive(Debug, Clone)]
pub struct Frame<F: Float> {
    pub state: State,
    pub buffer: Runtime<F>,
    pub header: Header<F>,
}

pub type HookFn<F> = Box<dyn Fn(Frame<F>) -> Result<TensorOp, TensorError> + Send + Sync>;
pub type HookMap<F> = HashMap<Hook, HookFn<F>>;

pub struct ModelRuntime<F: Float> {
    model: Model,
    state: State,
    hooks: Arc<HookMap<F>>,
    phantom: PhantomData<F>,
}

impl<F: Float> super::model::ModelRuntime for ModelRuntime<F> {
    #[inline]
    fn info(&self) -> ModelInfo {
        self.model.info.clone()
    }

    #[inline]
    fn state(&self) -> impl super::model::State + AsAny + 'static {
        self.state.clone()
    }

    #[inline]
    fn model(&self) -> impl Serialize + 'static {
        self.model.clone()
    }
}

impl<F: Float> ModelRuntime<F> {
    pub fn new(model: Model, num_batch: usize) -> Self {
        let context = model.context.clone();
        let info = model.info.clone();
        let state = {
            let shape = Shape::new(info.num_emb, 5 * info.num_layer, num_batch, 1);
            let data = (0..info.num_layer * num_batch)
                .map(|_| {
                    [
                        vec![0.0; info.num_emb],
                        vec![0.0; info.num_emb],
                        vec![0.0; info.num_emb],
                        vec![f32::MIN; info.num_emb],
                        vec![0.0; info.num_emb],
                    ]
                    .concat()
                })
                .collect_vec()
                .concat();
            let data = context.tensor_from_data(shape, data).unwrap();
            State {
                context,
                info,
                data,
            }
        };
        Self {
            model,
            state,
            hooks: Default::default(),
            phantom: PhantomData,
        }
    }

    pub fn new_with_hooks(model: Model, num_batch: usize, hooks: HookMap<F>) -> Self {
        Self {
            hooks: Arc::new(hooks),
            ..Self::new(model, num_batch)
        }
    }
}

fn turbo(num_token: usize) -> bool {
    num_token % MIN_TOKEN_CHUNK_SIZE == 0
}

fn hook_op<F: Float>(
    hooks: &HookMap<F>,
    hook: &Hook,
    frame: &Frame<F>,
) -> Result<TensorOp, TensorError> {
    match hooks.get(hook) {
        Some(f) => f(frame.clone()),
        None => Ok(TensorOp::empty()),
    }
}

impl<F: Float> JobBuilder<InferJob<F>> for ModelRuntime<F> {
    type Info = InferInfo;

    async fn build(&self, seed: Self::Info) -> Result<InferJob<F>> {
        let model = &self.model;
        let state = &self.state;
        let context = &model.context;
        let info = &model.info;
        let tensor = &model.tensor;

        let num_token = seed.num_token();

        let redirect = seed.redirect();
        let num_header = redirect.headers.len();

        let buffer = Runtime::<F>::new(context, info, num_token);
        let header = Header::<F>::new(context, info, num_header);
        let frame = Frame {
            state: state.clone(),
            buffer: buffer.clone(),
            header: header.clone(),
        };

        if num_token == 0 {
            let embed_device = match &tensor.embed.u {
                Some(_) => EmbedDevice::Gpu,
                None => EmbedDevice::Cpu,
            };
            return Ok(InferJob {
                commands: vec![],
                redirect,
                embed_device,
                embed: model.tensor.embed.w.clone(),
                tokens: buffer.tokens,
                cursors: buffer.cursors,
                input: buffer.input,
                output: header.head_o,
            });
        }

        #[cfg(feature = "async-build")]
        let mut tasks = tokio::task::JoinSet::new();
        #[cfg(not(feature = "async-build"))]
        let mut commands = vec![];

        let (head_ops, head_x) = if num_token == 1 || num_token == num_header {
            (vec![], buffer.ffn_x.clone())
        } else {
            let headers = &redirect.headers;
            let mut start = 0;
            let mut end = 1;
            let mut ops = vec![];
            while end <= headers.len() {
                if end == headers.len() || headers[end - 1] + 1 != headers[end] {
                    let first = headers[start];
                    let last = headers[end - 1];
                    assert_eq!(last - first + 1, end - start);

                    let input = buffer.ffn_x.view(.., first..=last, .., ..)?;
                    let output = header.head_x.view(.., start..end, .., ..)?;
                    ops.push(TensorOp::blit(input, output)?);

                    start = end;
                }
                end += 1;
            }
            (ops, header.head_x.clone())
        };

        let hook_op = |hook: Hook| hook_op(&self.hooks, &hook, &frame);

        let mut ops = vec![];
        let embed_device = match &tensor.embed.u {
            Some(u) => {
                ops.push(TensorOp::embed(&buffer.tokens, u, &buffer.input)?);
                EmbedDevice::Gpu
            }
            None => EmbedDevice::Cpu,
        };
        ops.append(&mut vec![
            hook_op(Hook::PostEmbedLoaded)?,
            TensorOp::layer_norm(
                &tensor.embed.layer_norm.w,
                &tensor.embed.layer_norm.b,
                &buffer.input,
                None,
                Model::LN_EPS,
            )?,
            hook_op(Hook::PostEmbedLayerNorm)?,
        ]);

        let mut id = PassId::new();

        {
            let context = context.clone();
            let id = id.inc();
            let f = move || -> Result<_> {
                let ops = TensorOp::List(ops);
                let mut encoder = context.device.create_command_encoder(&Default::default());
                let mut pass = encoder.begin_compute_pass(&Default::default());
                pass.execute_tensor_op(&ops);
                drop(pass);
                Ok((id, encoder.finish()))
            };
            #[cfg(feature = "async-build")]
            tasks.spawn_blocking(f);
            #[cfg(not(feature = "async-build"))]
            commands.push(f()?)
        }

        for (index, layer) in tensor.layers.iter().enumerate() {
            let context = context.clone();
            let id = id.inc();
            let hooks = self.hooks.clone();
            let frame = frame.clone();
            let layer = layer.clone();
            let f = move || -> Result<_> {
                Ok((
                    id,
                    build_layer(context, hooks, frame, layer, index, num_token)?,
                ))
            };
            #[cfg(feature = "async-build")]
            tasks.spawn_blocking(f);
            #[cfg(not(feature = "async-build"))]
            commands.push(f()?)
        }

        {
            let context = context.clone();
            let id = id.inc();
            let hooks = self.hooks.clone();
            let frame = frame.clone();
            let head = model.tensor.head.clone();
            let f = move || -> Result<_> {
                Ok((
                    id,
                    build_header(context, hooks, frame, head, head_x, num_header, head_ops)?,
                ))
            };
            #[cfg(feature = "async-build")]
            tasks.spawn_blocking(f);
            #[cfg(not(feature = "async-build"))]
            commands.push(f()?)
        }

        #[cfg(feature = "async-build")]
        let mut commands = vec![];
        #[cfg(feature = "async-build")]
        while let Some(result) = tasks.join_next().await {
            commands.push(result??);
        }
        let commands = commands
            .into_iter()
            .sorted_by_key(|x| x.0)
            .map(|x| x.1)
            .collect_vec();

        Ok(InferJob {
            commands,
            redirect,
            embed_device,
            embed: model.tensor.embed.w.clone(),
            tokens: buffer.tokens,
            cursors: buffer.cursors,
            input: buffer.input,
            output: header.head_o,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn build_layer<F: Float>(
    context: Context,
    hooks: Arc<HookMap<F>>,
    frame: Frame<F>,
    layer: Layer,
    index: usize,
    num_token: usize,
) -> Result<CommandBuffer> {
    let hook_op = |hook: Hook| hook_op(&hooks, &hook, &frame);
    let Frame { state, buffer, .. } = &frame;

    let info = &state.info;
    let mut encoder = context.device.create_command_encoder(&Default::default());

    encoder.copy_tensor(&buffer.input, &buffer.att_x)?;

    let ops = TensorOp::List(vec![
        hook_op(Hook::PreAtt(index))?,
        TensorOp::layer_norm(
            &layer.att_layer_norm.w,
            &layer.att_layer_norm.b,
            &buffer.att_x,
            None,
            Model::LN_EPS,
        )?,
        hook_op(Hook::PostAttLayerNorm(index))?,
        hook_op(Hook::PreAttTokenShift(index))?,
        TensorOp::token_shift(
            &buffer.cursors,
            layer.att.time_mix_k.view(.., .., .., ..)?,
            state.att(index)?,
            &buffer.att_x,
            &buffer.att_kx,
            false,
        )?,
        TensorOp::token_shift(
            &buffer.cursors,
            layer.att.time_mix_v.view(.., .., .., ..)?,
            state.att(index)?,
            &buffer.att_x,
            &buffer.att_vx,
            false,
        )?,
        TensorOp::token_shift(
            &buffer.cursors,
            layer.att.time_mix_r.view(.., .., .., ..)?,
            state.att(index)?,
            &buffer.att_x,
            &buffer.att_rx,
            false,
        )?,
        hook_op(Hook::PostAttTokenShift(index))?,
        hook_op(Hook::PreAttLinear(index))?,
        layer.att.w_k.matmul_op(
            buffer.att_kx.view(.., .., .., ..)?,
            buffer.att_k.view(.., .., .., ..)?,
            Activation::None,
            turbo(num_token),
        )?,
        layer.att.w_v.matmul_op(
            buffer.att_vx.view(.., .., .., ..)?,
            buffer.att_v.view(.., .., .., ..)?,
            Activation::None,
            turbo(num_token),
        )?,
        layer.att.w_r.matmul_op(
            buffer.att_rx.view(.., .., .., ..)?,
            buffer.att_r.view(.., .., .., ..)?,
            Activation::None,
            turbo(num_token),
        )?,
        hook_op(Hook::PostAttLinear(index))?,
        hook_op(Hook::PreAttTimeMix(index))?,
        TensorOp::blit(
            buffer.att_x.view(.., .., .., ..)?,
            buffer.aux_x.view(.., .., .., ..)?,
        )?,
        TensorOp::time_mix_v4(
            &buffer.cursors,
            &layer.att.time_decay,
            &layer.att.time_first,
            state.att(index)?,
            &buffer.att_k,
            &buffer.att_v,
            &buffer.att_r,
            &buffer.aux_x,
        )?,
        TensorOp::blit(
            buffer.aux_x.view(.., .., .., ..)?,
            buffer.att_x.view(.., .., .., ..)?,
        )?,
        hook_op(Hook::PostAttTimeMix(index))?,
        hook_op(Hook::PreAttOut(index))?,
        layer.att.w_o.matmul_op(
            buffer.att_x.view(.., .., .., ..)?,
            buffer.att_o.view(.., .., .., ..)?,
            Activation::None,
            turbo(num_token),
        )?,
        hook_op(Hook::PostAttOut(index))?,
        TensorOp::add(
            buffer.input.view(.., .., .., ..)?,
            buffer.att_o.view(.., .., .., ..)?,
        )?,
        hook_op(Hook::PostAtt(index))?,
    ]);

    {
        let mut pass = encoder.begin_compute_pass(&Default::default());
        pass.execute_tensor_op(&ops);
    }

    encoder.copy_tensor(&buffer.att_o, &buffer.ffn_x)?;

    let ops = TensorOp::List(vec![
        hook_op(Hook::PreFfn(index))?,
        TensorOp::layer_norm(
            &layer.ffn_layer_norm.w,
            &layer.ffn_layer_norm.b,
            &buffer.ffn_x,
            None,
            Model::LN_EPS,
        )?,
        hook_op(Hook::PostFfnLayerNorm(index))?,
        hook_op(Hook::PreFfnTokenShift(index))?,
        TensorOp::token_shift(
            &buffer.cursors,
            layer.ffn.time_mix_k.view(.., .., .., ..)?,
            state.ffn(index)?,
            &buffer.ffn_x,
            &buffer.ffn_kx,
            false,
        )?,
        TensorOp::token_shift(
            &buffer.cursors,
            layer.ffn.time_mix_r.view(.., .., .., ..)?,
            state.ffn(index)?,
            &buffer.ffn_x,
            &buffer.ffn_rx,
            false,
        )?,
        hook_op(Hook::PostFfnTokenShift(index))?,
        hook_op(Hook::PreFfnLinear(index))?,
        layer.ffn.w_k.matmul_op(
            buffer.ffn_kx.view(.., .., .., ..)?,
            buffer.ffn_k.view(.., .., .., ..)?,
            Activation::SquaredRelu,
            turbo(num_token),
        )?,
        hook_op(Hook::PostFfnActivate(index))?,
        layer.ffn.w_v.matmul_op(
            buffer.ffn_k.view(.., .., .., ..)?,
            buffer.ffn_v.view(.., .., .., ..)?,
            Activation::None,
            turbo(num_token),
        )?,
        layer.ffn.w_r.matmul_op(
            buffer.ffn_rx.view(.., .., .., ..)?,
            buffer.ffn_r.view(.., .., .., ..)?,
            Activation::None,
            turbo(num_token),
        )?,
        hook_op(Hook::PostFfnLinear(index))?,
        hook_op(Hook::PreFfnChannelMix(index))?,
        TensorOp::channel_mix(
            &buffer.cursors,
            state.ffn(index)?,
            &buffer.ffn_r,
            &buffer.ffn_v,
            &buffer.ffn_x,
        )?,
        hook_op(Hook::PostFfnChannelMix(index))?,
        TensorOp::add(
            buffer.att_o.view(.., .., .., ..)?,
            buffer.ffn_x.view(.., .., .., ..)?,
        )?,
        hook_op(Hook::PostFfn(index))?,
    ]);

    {
        let mut pass = encoder.begin_compute_pass(&Default::default());
        pass.execute_tensor_op(&ops);
    }

    if (index + 1) % Model::RESCALE_LAYER == 0 {
        let op = TensorOp::discount(&buffer.ffn_x, 0.5)?;
        let mut pass = encoder.begin_compute_pass(&Default::default());
        pass.execute_tensor_op(&op);
        drop(pass);
    }

    if index != info.num_layer - 1 {
        encoder.copy_tensor(&buffer.ffn_x, &buffer.input)?;
    }

    Ok(encoder.finish())
}

fn build_header<F: Float>(
    context: Context,
    hooks: Arc<HookMap<F>>,
    frame: Frame<F>,
    head: Head,
    head_x: TensorGpu<F, ReadWrite>,
    num_header: usize,
    mut ops: Vec<TensorOp>,
) -> Result<CommandBuffer> {
    let hook_op = |hook: Hook| hook_op(&hooks, &hook, &frame);
    let header = &frame.header;

    let mut encoder = context.device.create_command_encoder(&Default::default());
    if num_header > 0 {
        ops.append(&mut vec![
            hook_op(Hook::PreHead)?,
            TensorOp::layer_norm(
                &head.layer_norm.w,
                &head.layer_norm.b,
                &head_x,
                None,
                Model::LN_EPS,
            )?,
            hook_op(Hook::PostHeadLayerNorm)?,
            head.w.matmul_op(
                head_x.view(.., .., .., ..)?,
                header.head_o.view(.., .., .., ..)?,
                Activation::None,
                turbo(num_header),
            )?,
            hook_op(Hook::PostHead)?,
        ]);
        let ops = TensorOp::List(ops);

        let mut pass = encoder.begin_compute_pass(&Default::default());
        pass.execute_tensor_op(&ops);
    }
    Ok(encoder.finish())
}

impl<R: Reader> Build<Model> for ModelBuilder<R> {
    async fn build(self) -> Result<Model> {
        let ModelBuilder {
            context,
            model,
            lora,
            quant,
            embed_device,
        } = self;

        let info = Loader::info(&model)?;
        let loader = Loader {
            context: context.clone(),
            model,
            lora,
        };

        let embed = Embed {
            layer_norm: LayerNorm {
                w: loader.load_vector_f16("blocks.0.ln0.weight").await?,
                b: loader.load_vector_f16("blocks.0.ln0.bias").await?,
            },
            w: loader.load_embed().await?,
            u: match embed_device {
                EmbedDevice::Cpu => None,
                EmbedDevice::Gpu => Some(loader.load_matrix_f16("emb.weight").await?),
            },
        };

        let head = Head {
            layer_norm: LayerNorm {
                w: loader.load_vector_f16("ln_out.weight").await?,
                b: loader.load_vector_f16("ln_out.bias").await?,
            },
            w: Matrix::Fp16(loader.load_matrix_f16("head.weight").await?),
        };

        context.queue.submit(None);
        context.device.poll(wgpu::MaintainBase::Wait);

        let load_matrix = |name: String, quant: Quant| loader.load_matrix(name, quant);
        let load_matrix_discount = |name: String, quant: Quant, discount: f32| {
            loader.load_matrix_discount(name, quant, discount)
        };

        let mut layers = vec![];
        for layer in 0..info.num_layer {
            let quant = quant.get(&layer).copied().unwrap_or_default();
            let discount = 2.0_f32.powi(-((layer / Model::RESCALE_LAYER) as i32));

            let att_layer_norm = LayerNorm {
                w: loader
                    .load_vector_f16(format!("blocks.{layer}.ln1.weight"))
                    .await?,
                b: loader
                    .load_vector_f16(format!("blocks.{layer}.ln1.bias"))
                    .await?,
            };

            let att = format!("blocks.{layer}.att");
            let time_decay = loader
                .load_vector_exp_f32(format!("{att}.time_decay"))
                .await?;
            let time_first = loader.load_vector_f32(format!("{att}.time_first")).await?;
            let time_mix_k = loader.load_vector_f16(format!("{att}.time_mix_k")).await?;
            let time_mix_v = loader.load_vector_f16(format!("{att}.time_mix_v")).await?;
            let time_mix_r = loader.load_vector_f16(format!("{att}.time_mix_r")).await?;

            let att = Att {
                time_decay,
                time_first,
                time_mix_k,
                time_mix_v,
                time_mix_r,
                w_k: load_matrix(format!("{att}.key.weight"), quant).await?,
                w_v: load_matrix(format!("{att}.value.weight"), quant).await?,
                w_r: load_matrix(format!("{att}.receptance.weight"), quant).await?,
                w_o: load_matrix_discount(format!("{att}.output.weight"), quant, discount).await?,
            };

            let ffn_layer_norm = LayerNorm {
                w: loader
                    .load_vector_f16(format!("blocks.{layer}.ln2.weight"))
                    .await?,
                b: loader
                    .load_vector_f16(format!("blocks.{layer}.ln2.bias"))
                    .await?,
            };

            let ffn = format!("blocks.{layer}.ffn");
            let time_mix_k = loader.load_vector_f16(format!("{ffn}.time_mix_k")).await?;
            let time_mix_r = loader.load_vector_f16(format!("{ffn}.time_mix_r")).await?;

            let ffn = Ffn {
                time_mix_k,
                time_mix_r,
                w_r: load_matrix(format!("{ffn}.receptance.weight"), quant).await?,
                w_k: load_matrix(format!("{ffn}.key.weight"), quant).await?,
                w_v: load_matrix_discount(format!("{ffn}.value.weight"), quant, discount).await?,
            };

            context.queue.submit(None);
            context.device.poll(wgpu::MaintainBase::Wait);

            layers.push(Layer {
                att_layer_norm,
                ffn_layer_norm,
                att,
                ffn,
            })
        }

        context.queue.submit(None);
        context.device.poll(wgpu::MaintainBase::Wait);

        let tensor = ModelTensor {
            embed,
            head,
            layers,
        };
        let model = {
            let context = context.clone();
            let info = info.clone();
            Model {
                context,
                info,
                tensor,
            }
        };
        Ok(model)
    }
}
