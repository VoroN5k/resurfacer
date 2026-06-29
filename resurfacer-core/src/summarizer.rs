use std::time::Duration;

use crate::config::LlmConfig;

pub struct TabCluster {
    pub tabs: Vec<TabEntry>,
    pub reason: String,
}

pub struct TabEntry {
    pub url: String,
    pub title: Option<String>,
    pub extracted_text: Option<String>,
}

pub trait Summarizer: Send {
    fn summarize(&mut self, cluster: &TabCluster) -> String;
}

// NaiveSummarizer - always available, no external service required
pub struct NaiveSummarizer;

impl Summarizer for NaiveSummarizer {
    fn summarize(&mut self, cluster: &TabCluster) -> String {
        let titles: Vec<String> = cluster
            .tabs
            .iter()
            .map(|t| t.title.as_deref().unwrap_or(t.url.as_str()).to_string())
            .collect();
        format!(
            "{} tab{} - {}",
            cluster.tabs.len(),
            if cluster.tabs.len() == 1 { "" } else { "s" },
            titles.join(", ")
        )
    }
}

// OllamaSummarizer - calls a local Ollama server (http://localhost:11434)
// Falls back to NaiveSummarizer if Ollama is unreachable or returns an error
pub struct OllamaSummarizer {
    client: reqwest::blocking::Client,
    generate_url: String,
    model: String,
}

impl OllamaSummarizer {
    pub fn new(config: &LlmConfig) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("reqwest client");

        let base = config.ollama_url.trim_end_matches('/');
        Self {
            client,
            generate_url: format!("{base}/api/generate"),
            model: config.ollama_model.clone(),
        }
    }

    pub fn is_available(&self) -> bool {
        let tags_url = self.generate_url.replace("/api/generate", "/api/tags");
        self.client.get(&tags_url).send().is_ok()
    }

    fn build_prompt(&self, cluster: &TabCluster) -> String {
        let list: String = cluster
            .tabs
            .iter()
            .map(|t| {
                let title = t.title.as_deref().unwrap_or("untitled");
                format!("- {title} ({})", short_host(&t.url))
            })
            .collect::<Vec<_>>()
            .join("\n");

        format!(
            "<|im_start|>system\n\
             Ти підсумовуєш сесії браузерних вкладок одним реченням. Будь стислим і конкретним. Відповідай лише українською мовою.<|im_end|>\n\
             <|im_start|>user\n\
             Користувач мав відкриті ці вкладки браузера, але так і не переглянув їх:\n\
             {list}\n\
             Напиши рівно одне речення українською мовою, яке підсумовує, що він досліджував.<|im_end|>\n\
             <|im_start|>assistant\n"
        )
    }
}

impl Summarizer for OllamaSummarizer {
    fn summarize(&mut self, cluster: &TabCluster) -> String {
        let body = serde_json::json!({
            "model": self.model,
            "prompt": self.build_prompt(cluster),
            "stream": false,
            "options": {
                "temperature": 0.7,
                "num_predict": 120,
                "stop": ["<|im_end|>", "\n\n"],
            }
        });

        match self.client.post(&self.generate_url).json(&body).send() {
            Err(e) => {
                tracing::warn!("Ollama request failed: {e}");
                NaiveSummarizer.summarize(cluster)
            }
            Ok(resp) => {
                let summary = resp
                    .json::<serde_json::Value>()
                    .ok()
                    .and_then(|v| v["response"].as_str().map(|s| s.trim().to_string()))
                    .filter(|s| !s.is_empty());

                match summary {
                    Some(s) => s,
                    None => {
                        tracing::warn!("Empty Ollama response, falling back to naive summary");
                        NaiveSummarizer.summarize(cluster)
                    }
                }
            }
        }
    }
}

pub fn build_summarizer(config: &LlmConfig) -> Box<dyn Summarizer> {
    let s = OllamaSummarizer::new(config);
    if s.is_available() {
        tracing::info!(model = %s.model, "Using Ollama summarizer");
        Box::new(s)
    } else {
        tracing::info!(
            url = %s.generate_url,
            "Ollama not reachable, using naive summarizer"
        );
        Box::new(NaiveSummarizer)
    }
}

fn short_host(url: &str) -> &str {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(url)
}
