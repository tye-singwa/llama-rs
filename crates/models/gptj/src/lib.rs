//! An implementation of [GPT-J](https://huggingface.co/docs/transformers/model_doc/gptj) for the `llm` ecosystem.
#![deny(missing_docs)]

use std::error::Error;

use ggml::Tensor;
use llm_base::{
    ggml,
    model::{common, HyperparametersWriteError},
    util, FileType, InferenceParameters, InferenceSession, InferenceSessionConfig, KnownModel,
    LoadError, Mmap, ModelParameters, OutputRequest, Regex, TensorLoader, TokenId, Vocabulary,
};

/// The GPT-J model. Ref: [GitHub](https://github.com/kingoflolz/mesh-transformer-jax/#gpt-j-6b)
///
/// # Safety
/// This implements [Send] and [Sync] as it is immutable after construction.
pub struct GptJ {
    // the context size ("memory") the model should use when evaluating a prompt
    context_size: usize,

    hyperparameters: Hyperparameters,
    vocabulary: Vocabulary,

    // model-global weights
    // normalization gain & bias
    ln_f_g: Tensor,
    ln_f_b: Tensor,
    // weighted token embeddings
    wte: Tensor,
    // language model head gain & bias
    lmh_g: Tensor,
    lmh_b: Tensor,

    // weights for the model
    layers: Vec<Layer>,

    // must be kept alive for the model
    _context: ggml::Context,
    _mmap: Option<Mmap>,
}

unsafe impl Send for GptJ {}
unsafe impl Sync for GptJ {}

impl KnownModel for GptJ {
    type Hyperparameters = Hyperparameters;
    type Overrides = ();

    fn new<E: Error>(
        hyperparameters: Self::Hyperparameters,
        params: ModelParameters,
        _overrides: Option<Self::Overrides>,
        vocabulary: Vocabulary,
        tensor_loader: impl TensorLoader<E>,
    ) -> Result<Self, E>
    where
        Self: Sized,
    {
        let mut tl = tensor_loader;

        // model-global weights
        let wte = tl.load("transformer.wte.weight")?;
        let ln_f_g = tl.load("transformer.ln_f.weight")?;
        let ln_f_b = tl.load("transformer.ln_f.bias")?;
        let lmh_g = tl.load("lm_head.weight")?;
        let lmh_b = tl.load("lm_head.bias")?;

        let mut layers = Vec::new();
        for i in 0..hyperparameters.n_layer {
            let layer = Layer {
                ln_1_g: tl.load(&format!("transformer.h.{i}.ln_1.weight"))?,
                ln_1_b: tl.load(&format!("transformer.h.{i}.ln_1.bias"))?,
                c_attn_q_proj_w: tl.load(&format!("transformer.h.{i}.attn.q_proj.weight"))?,
                c_attn_k_proj_w: tl.load(&format!("transformer.h.{i}.attn.k_proj.weight"))?,
                c_attn_v_proj_w: tl.load(&format!("transformer.h.{i}.attn.v_proj.weight"))?,
                c_attn_proj_w: tl.load(&format!("transformer.h.{i}.attn.out_proj.weight"))?,
                c_mlp_fc_w: tl.load(&format!("transformer.h.{i}.mlp.fc_in.weight"))?,
                c_mlp_fc_b: tl.load(&format!("transformer.h.{i}.mlp.fc_in.bias"))?,
                c_mlp_proj_w: tl.load(&format!("transformer.h.{i}.mlp.fc_out.weight"))?,
                c_mlp_proj_b: tl.load(&format!("transformer.h.{i}.mlp.fc_out.bias"))?,
            };

            layers.push(layer);
        }

        let (_context, _, _mmap) = tl.finish();

        let ModelParameters { context_size, .. } = params;

        Ok(GptJ {
            hyperparameters,
            context_size,
            vocabulary,
            ln_f_g,
            ln_f_b,
            wte,
            lmh_g,
            lmh_b,
            layers,
            _mmap,
            _context,
        })
    }

    fn start_session(&self, config: InferenceSessionConfig) -> InferenceSession {
        InferenceSession::new(
            config,
            self.hyperparameters.n_ctx,
            self.hyperparameters.n_layer,
            self.hyperparameters.n_embd,
            self.hyperparameters.n_vocab,
        )
    }

    fn evaluate(
        &self,
        session: &mut InferenceSession,
        params: &InferenceParameters,
        input_tokens: &[TokenId],
        output_request: &mut OutputRequest,
    ) {
        let input_len = input_tokens.len();
        let session_len = session.n_past;
        let num_threads = params.n_threads;
        let ctx_size = self.context_size;

        let Hyperparameters {
            n_embd,
            n_head,
            n_vocab,
            n_layer,
            n_rot,
            ..
        } = self.hyperparameters;

        let (ctx0, embd) = common::prepare_for_evaluate(n_layer, session, input_tokens);

        let mut input_layer = ctx0.op_get_rows(&self.wte, &embd);

        let memory_k = &session.memory_k;
        let memory_k_size = memory_k.element_size();

        let memory_v = &session.memory_v;
        let memory_v_size = memory_v.element_size();

        let mut gf = ggml::ComputationGraph::new(num_threads);
        for il in 0..n_layer {
            // norm
            let mut current = ctx0.op_norm(&input_layer);
            current = ctx0.op_add(
                &ctx0.op_mul(&ctx0.op_repeat(&self.layers[il].ln_1_g, &current), &current),
                &ctx0.op_repeat(&self.layers[il].ln_1_b, &current),
            );

            let input_sa = current.share();

            // self-attention
            let qcur = ctx0.op_rope_inplace(
                &ctx0.op_reshape_3d(
                    &ctx0.op_mul_mat(&self.layers[il].c_attn_q_proj_w, &current),
                    n_embd / n_head,
                    n_head,
                    input_len,
                ),
                session_len,
                n_rot,
                0,
            );
            let kcur = ctx0.op_rope_inplace(
                &ctx0.op_reshape_3d(
                    &ctx0.op_mul_mat(&self.layers[il].c_attn_k_proj_w, &current),
                    n_embd / n_head,
                    n_head,
                    input_len,
                ),
                session_len,
                n_rot,
                0,
            );

            // self-attention store key and value to memory
            let vcur =
                ctx0.op_transpose(&ctx0.op_mul_mat(&self.layers[il].c_attn_v_proj_w, &current));

            let k = ctx0.op_view_1d(
                memory_k,
                input_len * n_embd,
                (memory_k_size * n_embd) * (il * ctx_size + session_len),
            );
            let v = ctx0.op_view_2d(
                memory_v,
                (input_len, n_embd),
                ctx_size * memory_v_size,
                (il * ctx_size) * memory_v_size * n_embd + session_len * memory_v_size,
            );

            gf.build_forward_expand(&ctx0.op_cpy(&kcur, &k));
            gf.build_forward_expand(&ctx0.op_cpy(&vcur, &v));

            let q = ctx0.op_permute(&qcur, (0, 2, 1, 3));
            let big_k = ctx0.op_permute(
                &ctx0.op_reshape_3d(
                    &ctx0.op_view_1d(
                        memory_k,
                        (session_len + input_len) * n_embd,
                        il * ctx_size * memory_k_size * n_embd,
                    ),
                    n_embd / n_head,
                    n_head,
                    session_len + input_len,
                ),
                (0, 2, 1, 3),
            );

            let kq = ctx0.op_mul_mat(&big_k, &q);
            let kq_scaled = ctx0.op_scale_inplace(
                &kq,
                &ctx0.new_f32(1f32 / f32::sqrt(n_embd as f32 / n_head as f32)),
            );

            let kq_masked = ctx0.op_diag_mask_inf_inplace(&kq_scaled, session_len);
            let kq_softmax = ctx0.op_soft_max_inplace(&kq_masked);

            let big_v = ctx0.op_view_3d(
                memory_v,
                (session_len + input_len, n_embd / n_head, n_head),
                (
                    ctx_size * memory_v_size,
                    ctx_size * memory_v_size * n_embd / n_head,
                ),
                il * ctx_size * memory_v_size * n_embd,
            );

            let kqv = ctx0.op_mul_mat(&big_v, &kq_softmax);
            let kqv_merged = ctx0.op_permute(&kqv, (0, 2, 1, 3));

            current = ctx0.op_cpy(
                &kqv_merged,
                &ctx0.new_tensor_2d(ggml::Type::F32, n_embd, input_len),
            );

            // self-attention projection
            current = ctx0.op_mul_mat(&self.layers[il].c_attn_proj_w, &current);

            // feed-forward
            let ff_in = current.share();

            current = ctx0.op_mul_mat(&self.layers[il].c_mlp_fc_w, &input_sa);
            current = ctx0.op_add(
                &ctx0.op_repeat(&self.layers[il].c_mlp_fc_b, &current),
                &current,
            );

            current = ctx0.op_gelu(&current);

            // feed-forward projection
            current = ctx0.op_mul_mat(&self.layers[il].c_mlp_proj_w, &current);
            current = ctx0.op_add(
                &ctx0.op_repeat(&self.layers[il].c_mlp_proj_b, &current),
                &current,
            );

            current = ctx0.op_add(&current, &ff_in);

            // input for next layer
            input_layer = ctx0.op_add(&current, &input_layer);
        }

        // norm
        input_layer = ctx0.op_norm(&input_layer);
        input_layer = ctx0.op_add(
            &ctx0.op_mul(&ctx0.op_repeat(&self.ln_f_g, &input_layer), &input_layer),
            &ctx0.op_repeat(&self.ln_f_b, &input_layer),
        );

        // lm_head
        input_layer = ctx0.op_mul_mat(&self.lmh_g, &input_layer);
        input_layer = ctx0.op_add(&ctx0.op_repeat(&self.lmh_b, &input_layer), &input_layer);

        // run the computation
        gf.build_forward_expand(&input_layer);
        ctx0.graph_compute(&mut gf);

        // finish evaluation
        common::read_last_token(session, &input_layer, n_vocab, input_len);
        common::extract_logits(output_request, &input_layer, n_vocab, input_len);
        common::extract_embeddings(output_request, &embd, n_embd, input_len);
        common::update_session(session, &ctx0, input_tokens.len(), input_len);
    }

    fn vocabulary(&self) -> &Vocabulary {
        &self.vocabulary
    }

    fn context_size(&self) -> usize {
        self.context_size
    }

    fn bot_token_id(&self) -> Option<TokenId> {
        None
    }

    fn eot_token_id(&self) -> TokenId {
        self.vocabulary
            .token_to_id
            .get("<|endoftext|>".as_bytes())
            .copied()
            .unwrap()
    }

    fn quantize_tensors() -> Vec<Regex> {
        vec![Regex::new(".*weight").unwrap()]
    }

    fn skip_quantize_tensors() -> Vec<Regex> {
        vec![]
    }
}

/// GPT-J [hyperparameters](https://en.wikipedia.org/wiki/Hyperparameter_(machine_learning))
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct Hyperparameters {
    /// Size of the model's vocabulary
    pub n_vocab: usize,
    /// Size of the model's context
    pub n_ctx: usize,
    /// Size of the model's embedding layer
    pub n_embd: usize,
    /// n_head
    pub n_head: usize,
    /// Number of layers in the model
    pub n_layer: usize,
    /// n_rot
    pub n_rot: usize,
    /// file_type
    pub file_type: FileType,
}

impl llm_base::Hyperparameters for Hyperparameters {
    fn read_ggml(reader: &mut dyn std::io::BufRead) -> Result<Self, LoadError> {
        let hyperparameters = Hyperparameters {
            n_vocab: util::read_i32(reader)?.try_into()?,
            n_ctx: util::read_i32(reader)?.try_into()?,
            n_embd: util::read_i32(reader)?.try_into()?,
            n_head: util::read_i32(reader)?.try_into()?,
            n_layer: util::read_i32(reader)?.try_into()?,
            n_rot: util::read_i32(reader)?.try_into()?,
            file_type: util::read_filetype(reader)?,
        };

        let n_vocab = util::read_i32(reader)? as usize;
        if hyperparameters.n_vocab != n_vocab {
            return Err(LoadError::InvariantBroken {
                path: None,
                invariant: format!(
                    "GPT2 model expected n_vocab {} found {}",
                    hyperparameters.n_vocab, n_vocab
                ),
            });
        }

        Ok(hyperparameters)
    }

    fn write_ggml(&self, writer: &mut dyn std::io::Write) -> Result<(), HyperparametersWriteError> {
        util::write_i32(writer, self.n_vocab.try_into()?)?;
        util::write_i32(writer, self.n_ctx.try_into()?)?;
        util::write_i32(writer, self.n_embd.try_into()?)?;
        util::write_i32(writer, self.n_head.try_into()?)?;
        util::write_i32(writer, self.n_layer.try_into()?)?;
        util::write_i32(writer, self.n_rot.try_into()?)?;
        util::write_i32(writer, self.file_type.into())?;
        Ok(())
    }

    fn n_vocabulary(&self) -> usize {
        self.n_vocab
    }

    fn file_type(&self) -> Option<FileType> {
        Some(self.file_type)
    }

    fn file_type_mut(&mut self) -> Option<&mut FileType> {
        Some(&mut self.file_type)
    }
}

struct Layer {
    // normalization
    ln_1_g: Tensor,
    ln_1_b: Tensor,

    // attention
    c_attn_q_proj_w: Tensor,
    c_attn_k_proj_w: Tensor,
    c_attn_v_proj_w: Tensor,

    c_attn_proj_w: Tensor,

    // ff
    c_mlp_fc_w: Tensor,
    c_mlp_fc_b: Tensor,

    c_mlp_proj_w: Tensor,
    c_mlp_proj_b: Tensor,
}
