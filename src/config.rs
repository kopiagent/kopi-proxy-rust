use std::collections::HashMap;
use std::env;

/// An upstream provider configuration.
#[derive(Debug, Clone)]
pub struct Upstream {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
}

/// A single model mapping: which upstream + what model name to send.
#[derive(Debug, Clone)]
pub struct ModelRoute {
    pub upstream: String,
    pub model: String,
    /// Capability tier: 1=strongest(kopi-o-pro), 2=standard(kopi-o), 3=fast(kopi-flash)
    pub tier: u8,
}

/// All runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub db_path: String,
    pub brand: String,
    pub upstreams: HashMap<String, Upstream>,
    pub model_map: HashMap<String, ModelRoute>,
    pub rate_limit_rpm: u32,
    /// Admin API key for billing management backend.
    pub admin_key: String,
    /// Ordered fallback chain: when an upstream fails, try these in order.
    /// Each entry is (upstream_name, model_name) — model_name is the upstream's native model name.
    pub global_fallback: Vec<(String, String)>,
}

/// Smart routing heuristics result
#[derive(Debug, Clone, PartialEq)]
pub struct RoutingHint {
    pub has_images: bool,
    pub has_code: bool,
    pub needs_reasoning: bool,
    pub estimated_tokens: usize,
    pub is_long_context: bool,
}

impl Config {
    pub fn from_env() -> Self {
        let mut upstreams = HashMap::new();

        // MiMo (original)
        upstreams.insert(
            "mimo".to_string(),
            Upstream {
                name: "mimo".into(),
                base_url: env::var("MIMO_BASE_URL")
                    .unwrap_or_else(|_| "https://token-plan-sgp.xiaomimimo.com/v1".into()),
                api_key: env::var("MIMO_API_KEY")
                    .unwrap_or_default(),
            },
        );

        // MiMo2
        upstreams.insert(
            "mimo2".to_string(),
            Upstream {
                name: "mimo2".into(),
                base_url: env::var("MIMO2_BASE_URL")
                    .unwrap_or_else(|_| "https://token-plan-sgp.xiaomimimo.com/v1".into()),
                api_key: env::var("MIMO2_API_KEY")
                    .unwrap_or_default(),
            },
        );

        // MiMo3 (primary)
        upstreams.insert(
            "mimo3".to_string(),
            Upstream {
                name: "mimo3".into(),
                base_url: env::var("MIMO3_BASE_URL")
                    .unwrap_or_else(|_| "https://token-plan-sgp.xiaomimimo.com/v1".into()),
                api_key: env::var("MIMO3_API_KEY")
                    .unwrap_or_default(),
            },
        );

        // DeepSeek
        upstreams.insert(
            "deepseek".to_string(),
            Upstream {
                name: "deepseek".into(),
                base_url: env::var("DEEPSEEK_BASE_URL")
                    .unwrap_or_else(|_| "https://api.deepseek.com".into()),
                api_key: env::var("DEEPSEEK_API_KEY")
                    .unwrap_or_default(),
            },
        );

        // NVIDIA
        upstreams.insert(
            "nvidia".to_string(),
            Upstream {
                name: "nvidia".into(),
                base_url: env::var("NVIDIA_BASE_URL")
                    .unwrap_or_else(|_| "https://integrate.api.nvidia.com/v1".into()),
                api_key: env::var("NVIDIA_API_KEY")
                    .unwrap_or_default(),
            },
        );

        // OpenRouter (real key — verified working 2025-07-09)
        upstreams.insert(
            "openrouter".to_string(),
            Upstream {
                name: "openrouter".into(),
                base_url: env::var("OPENROUTER_BASE_URL")
                    .unwrap_or_else(|_| "https://openrouter.ai/api/v1".into()),
                api_key: env::var("OPENROUTER_API_KEY")
                    .unwrap_or_else(|_| "".into()),
            },
        );

        // z.ai (GLM-5.2 for kopi-o-pro)
        upstreams.insert(
            "zai".to_string(),
            Upstream {
                name: "zai".into(),
                base_url: env::var("ZAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.z.ai/api/coding/paas/v4".into()),
                api_key: env::var("ZAI_API_KEY")
                    .unwrap_or_default(),
            },
        );

        // KOPI MCP (Hunyuan 45.32.109.144:8933) — Grok upstream (grok-4.3, grok-4.20-reasoning, grok-vision)
        upstreams.insert(
            "kopi_mcp".to_string(),
            Upstream {
                name: "kopi_mcp".into(),
                base_url: env::var("KOPI_MCP_BASE_URL")
                    .unwrap_or_else(|_| "http://45.32.109.144:8933/v1".into()),
                api_key: env::var("KOPI_MCP_API_KEY")
                    .unwrap_or_else(|_| "no-key-needed".into()),
            },
        );

        // Model map — matches server.py MODEL_MAP exactly
        // tier: 1=最强推理, 2=标准强模型, 3=快速日常
        let mut model_map = HashMap::new();
        model_map.insert("kopi-flash".into(),            ModelRoute { upstream: "deepseek".into(), model: "deepseek-v4-pro".into(),            tier: 3 });
        model_map.insert("kopi-gau".into(),              ModelRoute { upstream: "mimo3".into(),    model: "mimo-v2.5-pro".into(),             tier: 2 });
        model_map.insert("kopi-grok".into(),             ModelRoute { upstream: "kopi_mcp".into(), model: "grok-4.3".into(),                  tier: 2 });
        model_map.insert("kopi-o-pro".into(),            ModelRoute { upstream: "zai".into(),      model: "glm-5.2".into(),                  tier: 1 });
        model_map.insert("kopi-glm52".into(),            ModelRoute { upstream: "zai".into(),      model: "glm-5.2".into(),                  tier: 1 });
        model_map.insert("kopi-o".into(),                ModelRoute { upstream: "mimo3".into(),    model: "mimo-v2.5-pro".into(),             tier: 2 });
        model_map.insert("kopi-siew-dai".into(),         ModelRoute { upstream: "mimo3".into(),    model: "mimo-v2.5-pro".into(),             tier: 2 });
        model_map.insert("kopi-nemotron-ultra".into(),   ModelRoute { upstream: "nvidia".into(),    model: "nvidia/llama-3.3-nemotron-super-49b-v1.5".into(), tier: 2 });
        model_map.insert("kopi-nemotron".into(),          ModelRoute { upstream: "nvidia".into(),    model: "nvidia/llama-3.3-nemotron-super-49b-v1".into(),     tier: 3 });
        model_map.insert("kopi-o-flash".into(),           ModelRoute { upstream: "mimo3".into(),    model: "mimo-v2.5".into(),                 tier: 3 });
        model_map.insert("kopi-siew-dai-flash".into(),   ModelRoute { upstream: "mimo3".into(),    model: "mimo-v2.5-pro".into(),             tier: 3 });

        // Also register upstream model names directly for passthrough
        model_map.insert("deepseek-v4-pro".into(),       ModelRoute { upstream: "deepseek".into(), model: "deepseek-v4-pro".into(),           tier: 3 });
        model_map.insert("mimo-v2.5-pro".into(),          ModelRoute { upstream: "mimo3".into(),    model: "mimo-v2.5-pro".into(),             tier: 2 });
        model_map.insert("mimo-v2.5".into(),              ModelRoute { upstream: "mimo3".into(),    model: "mimo-v2.5".into(),                 tier: 3 });

        // OpenRouter models — verified working IDs (2025-07-09)
        model_map.insert("kopi-gpt4o".into(),          ModelRoute { upstream: "openrouter".into(), model: "openai/gpt-4o".into(),                 tier: 2 });
        model_map.insert("kopi-claude-opus".into(),     ModelRoute { upstream: "openrouter".into(), model: "anthropic/claude-opus-4.6".into(),     tier: 1 });
        model_map.insert("kopi-claude-sonnet".into(),   ModelRoute { upstream: "openrouter".into(), model: "anthropic/claude-sonnet-4.6".into(),   tier: 2 });
        model_map.insert("kopi-o3".into(),              ModelRoute { upstream: "openrouter".into(), model: "openai/o3".into(),                     tier: 1 });
        model_map.insert("kopi-gemini-pro".into(),      ModelRoute { upstream: "openrouter".into(), model: "google/gemini-2.5-pro".into(),         tier: 2 });
        model_map.insert("kopi-gemini-flash".into(),    ModelRoute { upstream: "openrouter".into(), model: "google/gemini-2.5-flash".into(),       tier: 3 });
        model_map.insert("kopi-grok-pro".into(),        ModelRoute { upstream: "openrouter".into(), model: "x-ai/grok-4.5".into(),                 tier: 2 });
        model_map.insert("kopi-ds-r1".into(),           ModelRoute { upstream: "openrouter".into(), model: "deepseek/deepseek-r1-0528".into(),     tier: 2 });
        model_map.insert("kopi-gpt5".into(),            ModelRoute { upstream: "openrouter".into(), model: "openai/gpt-5.5".into(),              tier: 1 });
        model_map.insert("kopi-gpt-5-5".into(),        ModelRoute { upstream: "openrouter".into(), model: "openai/gpt-5.5".into(),              tier: 1 });
        model_map.insert("kopi-qwen".into(),            ModelRoute { upstream: "openrouter".into(), model: "qwen/qwen3.7-max".into(),             tier: 2 });
        model_map.insert("kopi-kimi".into(),            ModelRoute { upstream: "openrouter".into(), model: "moonshotai/kimi-k2.7-code".into(),    tier: 2 });
        model_map.insert("kopi-gemini".into(),          ModelRoute { upstream: "openrouter".into(), model: "google/gemini-3.5-flash".into(),       tier: 3 });

        // Image generation models
        model_map.insert("kopi-gemini-image".into(), ModelRoute { upstream: "openrouter".into(), model: "google/gemini-3.1-flash-image".into(), tier: 2 });
        model_map.insert("kopi-gpt-image2".into(), ModelRoute { upstream: "openrouter".into(), model: "openai/gpt-5.4-image-2".into(), tier: 2 });

        // MiMo ASR/TTS — 音频模型 (passthrough to mimo3)
        model_map.insert("kopi-asr".into(),               ModelRoute { upstream: "mimo3".into(),    model: "mimo-v2.5-asr".into(),              tier: 2 });
        model_map.insert("kopi-tts".into(),               ModelRoute { upstream: "mimo3".into(),    model: "mimo-v2.5-tts".into(),              tier: 2 });
        model_map.insert("kopi-tts-voiceclone".into(),    ModelRoute { upstream: "mimo3".into(),    model: "mimo-v2.5-tts-voiceclone".into(),   tier: 2 });
        model_map.insert("kopi-tts-voicedesign".into(),   ModelRoute { upstream: "mimo3".into(),    model: "mimo-v2.5-tts-voicedesign".into(),  tier: 2 });
        // Also register upstream names for passthrough
        model_map.insert("mimo-v2.5-asr".into(),           ModelRoute { upstream: "mimo3".into(),    model: "mimo-v2.5-asr".into(),              tier: 2 });
        model_map.insert("mimo-v2.5-tts".into(),           ModelRoute { upstream: "mimo3".into(),    model: "mimo-v2.5-tts".into(),              tier: 2 });
        model_map.insert("mimo-v2.5-tts-voiceclone".into(), ModelRoute { upstream: "mimo3".into(),  model: "mimo-v2.5-tts-voiceclone".into(),   tier: 2 });
        model_map.insert("mimo-v2.5-tts-voicedesign".into(), ModelRoute { upstream: "mimo3".into(),  model: "mimo-v2.5-tts-voicedesign".into(),  tier: 2 });

        // ═══════════════════════════════════════════════════════════════════
        // FREE OpenRouter models ($0/M tokens)
        // ═══════════════════════════════════════════════════════════════════
        model_map.insert("kopi-qwen-coder".into(),     ModelRoute { upstream: "openrouter".into(), model: "qwen/qwen3-coder:free".into(),                     tier: 3 });
        model_map.insert("kopi-nemotron-ultra-free".into(), ModelRoute { upstream: "openrouter".into(), model: "nvidia/nemotron-3-ultra-550b-a55b:free".into(), tier: 3 });
        model_map.insert("kopi-nemotron-super-free".into(), ModelRoute { upstream: "openrouter".into(), model: "nvidia/nemotron-3-super-120b-a12b:free".into(), tier: 3 });
        model_map.insert("kopi-hy3".into(),             ModelRoute { upstream: "openrouter".into(), model: "tencent/hy3:free".into(),                          tier: 3 });
        model_map.insert("kopi-laguna".into(),          ModelRoute { upstream: "openrouter".into(), model: "poolside/laguna-xs-2.1:free".into(),               tier: 3 });
        model_map.insert("kopi-gemma4".into(),          ModelRoute { upstream: "openrouter".into(), model: "google/gemma-4-31b-it:free".into(),                tier: 3 });
        model_map.insert("kopi-gemma4-small".into(),    ModelRoute { upstream: "openrouter".into(), model: "google/gemma-4-26b-a4b-it:free".into(),            tier: 3 });
        model_map.insert("kopi-qwen-next".into(),       ModelRoute { upstream: "openrouter".into(), model: "qwen/qwen3-next-80b-a3b-instruct:free".into(),    tier: 3 });
        model_map.insert("kopi-north-code".into(),      ModelRoute { upstream: "openrouter".into(), model: "cohere/north-mini-code:free".into(),               tier: 3 });
        model_map.insert("kopi-nemotron-nano".into(),   ModelRoute { upstream: "openrouter".into(), model: "nvidia/nemotron-3-nano-30b-a3b:free".into(),       tier: 3 });
        model_map.insert("kopi-nemotron-reason".into(), ModelRoute { upstream: "openrouter".into(), model: "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free".into(), tier: 3 });
        model_map.insert("kopi-gpt-oss".into(),         ModelRoute { upstream: "openrouter".into(), model: "openai/gpt-oss-20b:free".into(),                  tier: 3 });
        model_map.insert("kopi-llama3-free".into(),     ModelRoute { upstream: "openrouter".into(), model: "meta-llama/llama-3.3-70b-instruct:free".into(),   tier: 3 });
        model_map.insert("kopi-hermes3".into(),         ModelRoute { upstream: "openrouter".into(), model: "nousresearch/hermes-3-llama-3.1-405b:free".into(), tier: 3 });
        model_map.insert("kopi-nemotron-vl".into(),     ModelRoute { upstream: "openrouter".into(), model: "nvidia/nemotron-nano-12b-v2-vl:free".into(),      tier: 3 });
        model_map.insert("kopi-venice".into(),          ModelRoute { upstream: "openrouter".into(), model: "cognitivecomputations/dolphin-mistral-24b-venice-edition:free".into(), tier: 3 });

        // ═══════════════════════════════════════════════════════════════════
        // CHEAP OpenRouter models (< $0.50/M tokens)
        // ═══════════════════════════════════════════════════════════════════
        model_map.insert("kopi-mistral-nemo".into(),   ModelRoute { upstream: "openrouter".into(), model: "mistralai/mistral-nemo".into(),                    tier: 3 });
        model_map.insert("kopi-granite".into(),        ModelRoute { upstream: "openrouter".into(), model: "ibm-granite/granite-4.1-8b".into(),                tier: 3 });
        model_map.insert("kopi-qwen35".into(),         ModelRoute { upstream: "openrouter".into(), model: "qwen/qwen3.5-9b".into(),                          tier: 3 });
        model_map.insert("kopi-ds-flash".into(),       ModelRoute { upstream: "openrouter".into(), model: "deepseek/deepseek-v4-flash".into(),                tier: 3 });
        model_map.insert("kopi-llama4".into(),         ModelRoute { upstream: "openrouter".into(), model: "meta-llama/llama-4-scout".into(),                  tier: 3 });
        model_map.insert("kopi-gemma3".into(),         ModelRoute { upstream: "openrouter".into(), model: "google/gemma-3-27b-it".into(),                    tier: 3 });
        model_map.insert("kopi-seed-flash".into(),     ModelRoute { upstream: "openrouter".into(), model: "bytedance-seed/seed-1.6-flash".into(),             tier: 3 });
        model_map.insert("kopi-step".into(),           ModelRoute { upstream: "openrouter".into(), model: "stepfun/step-3.5-flash".into(),                   tier: 3 });
        model_map.insert("kopi-qwen-vl".into(),        ModelRoute { upstream: "openrouter".into(), model: "qwen/qwen3-vl-32b-instruct".into(),               tier: 3 });
        model_map.insert("kopi-hermes4".into(),        ModelRoute { upstream: "openrouter".into(), model: "nousresearch/hermes-4-70b".into(),                 tier: 3 });
        model_map.insert("kopi-ds-v3".into(),          ModelRoute { upstream: "openrouter".into(), model: "deepseek/deepseek-v3.2".into(),                   tier: 3 });
        model_map.insert("kopi-phi4".into(),           ModelRoute { upstream: "openrouter".into(), model: "microsoft/phi-4".into(),                          tier: 3 });
        model_map.insert("kopi-ling".into(),           ModelRoute { upstream: "openrouter".into(), model: "inclusionai/ling-2.6-flash".into(),                tier: 3 });
        model_map.insert("kopi-llama-vision".into(),   ModelRoute { upstream: "openrouter".into(), model: "meta-llama/llama-3.2-11b-vision-instruct".into(), tier: 3 });

        // Smart fallback chain: ordered by reliability + cost.
        // mimo1/mimo2 keys expired (401) — excluded.
        let global_fallback: Vec<(String, String)> = vec![
            ("mimo3".into(), "mimo-v2.5-pro".into()),
            ("zai".into(), "glm-5.2".into()),
            ("deepseek".into(), "deepseek-v4-pro".into()),
            ("openrouter".into(), "google/gemini-2.5-flash".into()),
            ("nvidia".into(), "nvidia/llama-3.3-nemotron-super-49b-v1.5".into()),
        ];

        let admin_key = env::var("ADMIN_API_KEY")
            .unwrap_or_else(|_| "kopi-admin-default-key-change-me".into());

        Self {
            port: env::var("PORT")
                .unwrap_or_else(|_| "5100".into())
                .parse()
                .unwrap_or(5100),
            db_path: env::var("DB_PATH")
                .unwrap_or_else(|_| "/opt/kopi-proxy/data/kopi.db".into()),
            brand: "KOPI AI AGENT by Kopi Ai Agent Pte Ltd (Singapore)".into(),
            upstreams,
            model_map,
            rate_limit_rpm: env::var("RATE_LIMIT_RPM")
                .unwrap_or_else(|_| "60".into())
                .parse()
                .unwrap_or(60),
            admin_key,
            global_fallback,
        }
    }

    /// Analyze the request payload and return routing hints.
    pub fn analyze_payload(payload: &serde_json::Value) -> RoutingHint {
        let messages = payload.get("messages")
            .and_then(|m| m.as_array())
            .map(|a| a.as_slice())
            .unwrap_or_default();

        let total_chars: usize = messages.iter()
            .filter_map(|m| m.get("content"))
            .filter_map(|c| c.as_str())
            .map(|s| s.len())
            .sum();

        let combined: String = messages.iter()
            .filter_map(|m| m.get("content"))
            .filter_map(|c| c.as_str())
            .collect();

        // Check for images (multi-modal content)
        let has_images = messages.iter().any(|m| {
            m.get("content")
                .and_then(|c| c.as_array())
                .map(|arr| arr.iter().any(|part| {
                    part.get("type").and_then(|t| t.as_str()) == Some("image_url")
                }))
                .unwrap_or(false)
        });

        // Check for code patterns
        let code_indicators = [
            "```", "def ", "fn ", "impl ", "class ", "pub fn",
            "function ", "const ", "let ", "var ", "import ",
            "#include", "package ", "use std", "trait ", "enum ",
        ];
        let has_code = code_indicators.iter().any(|kw| combined.contains(kw));

        // Check for reasoning/complexity indicators
        let reasoning_indicators = [
            "explain", "analyze", "compare", "why", "how does",
            "prove", "design", "architecture", "algorithm",
            "review", "refactor", "strategy", "evaluate",
            "optimization", "trade-off", " 思路", "为什么",
            "分析", "比较", "设计", "解释",
        ];
        let lower = combined.to_lowercase();
        let needs_reasoning = reasoning_indicators.iter().any(|kw| lower.contains(kw));

        // Estimate tokens (chars/2.5 for mixed CJK+English)
        let estimated_tokens = (total_chars as f64 / 2.5).ceil() as usize;
        let is_long_context = total_chars > 8000;

        RoutingHint {
            has_images,
            has_code,
            needs_reasoning,
            estimated_tokens,
            is_long_context,
        }
    }

    /// Smart model selection based on payload analysis.
    pub fn smart_select_model(&self, hint: &RoutingHint) -> String {
        // Priority:
        // 1. Has images → need vision-capable model (mimo3 supports multimodal)
        // 2. Complex reasoning → kopi-o-pro (GLM-5.2)
        // 3. Code or long context → kopi-o (mimo-v2.5-pro, balanced)
        // 4. Simple chat → kopi-flash (deepseek-v4-pro, fastest)

        if hint.has_images {
            return "kopi-o".into(); // mimo3/mimo-v2.5-pro supports vision
        }
        if hint.needs_reasoning && hint.has_code {
            return "kopi-o-pro".into(); // strongest reasoning
        }
        if hint.needs_reasoning || hint.is_long_context {
            return "kopi-o".into(); // standard strong model
        }
        if hint.has_code {
            return "kopi-o".into(); // coding needs decent model
        }
        if hint.estimated_tokens > 2000 {
            return "kopi-o".into();
        }
        // Simple chat / quick Q&A → use fastest model
        "kopi-flash".into()
    }

    /// Build the failover attempt list: [primary, ...fallbacks excluding primary's own upstream].
    pub fn build_attempt_list(&self, upstream_name: &str, upstream_model: &str)
        -> Vec<(String, String)> {
        let mut attempts = Vec::new();
        attempts.push((upstream_name.to_string(), upstream_model.to_string()));
        for (fb_upstream, fb_model) in &self.global_fallback {
            if *fb_upstream != upstream_name {
                attempts.push((fb_upstream.clone(), fb_model.clone()));
            }
        }
        attempts
    }
}
