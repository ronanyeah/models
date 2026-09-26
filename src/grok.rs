use anyhow::Context;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};

use crate::xai::{
    GenerateImageRequest, GeneratedImage, GetEndpoint, GetModels, ImageAspectRatio,
    ImageResolution, ListModelsResponse, ModelInput, ModelInputContent, ModelInputPart,
    ModelOutput, ModelRequest, ModelResponse, ModelResponseConfiguration, ModelResponseFormat,
    ModelTool, OutputMessageContent, PostEndpoint,
};

/// Defaults; override per client with [`GrokClient::with_model`] and
/// [`GrokClient::with_image_model`]. Model ids aren't in the spec -- the live
/// list comes from [`GrokClient::fetch_models`].
const MODEL: &str = "grok-4-1-fast-reasoning";
const IMAGE_MODEL: &str = "grok-imagine-image-quality";

/// Hand-written because the spec has no `servers` block; the paths themselves
/// come from the generated endpoint impls.
const BASE_URL: &str = "https://api.x.ai";

/// Local conveniences on the generated image type.
pub trait GeneratedImageExt {
    /// Decode the inline base64 payload.
    fn bytes(&self) -> anyhow::Result<Vec<u8>>;

    /// File extension implied by `mime_type`, falling back to `jpg` when the
    /// API doesn't say.
    fn extension(&self) -> &'static str;
}

impl GeneratedImageExt for GeneratedImage {
    fn bytes(&self) -> anyhow::Result<Vec<u8>> {
        let b64 = self
            .b64_json
            .as_deref()
            .context("image response missing b64_json")?;

        Ok(base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            b64,
        )?)
    }

    fn extension(&self) -> &'static str {
        match self.mime_type.as_deref() {
            Some("image/png") => "png",
            Some("image/webp") => "webp",
            Some("image/gif") => "gif",
            _ => "jpg",
        }
    }
}

/// The `web_search` server-side tool, options left at their defaults.
fn web_search() -> ModelTool {
    ModelTool::WebSearch {
        allowed_domains: None,
        enable_image_search: None,
        enable_image_understanding: None,
        excluded_domains: None,
        // OpenAI-compatibility only; the request is rejected if set.
        external_web_access: None,
        filters: None,
        search_context_size: None,
        user_location: None,
    }
}

/// The `x_search` server-side tool, options left at their defaults.
fn x_search() -> ModelTool {
    ModelTool::XSearch {
        allowed_x_handles: None,
        enable_image_understanding: None,
        enable_video_understanding: None,
        excluded_x_handles: None,
        from_date: None,
        to_date: None,
    }
}

pub struct GrokClient {
    api_key: String,
    client: reqwest::Client,
    model: String,
    image_model: String,
    web_search: bool,
    x_search: bool,
}

impl GrokClient {
    pub fn new(api_key: &str) -> Self {
        Self {
            api_key: api_key.to_string(),

            client: reqwest::Client::new(),
            model: MODEL.to_string(),
            image_model: IMAGE_MODEL.to_string(),
            web_search: false,
            x_search: false,
        }
    }

    /// Offer the `web_search` tool, letting the model look things up on the
    /// web instead of recalling them. Off by default; billed per search.
    pub fn set_web_search(&mut self, enabled: bool) {
        self.web_search = enabled;
    }

    /// Offer the `x_search` tool. Off by default; billed per post and profile
    /// fetched, not per search.
    pub fn set_x_search(&mut self, enabled: bool) {
        self.x_search = enabled;
    }

    /// Use a different model for prompts.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Use a different model for image generation.
    ///
    /// Aspect ratio and resolution are only honoured by `grok-imagine` models;
    /// anything else silently ignores them.
    pub fn with_image_model(mut self, model: impl Into<String>) -> Self {
        self.image_model = model.into();
        self
    }

    pub async fn fetch_models(&self) -> anyhow::Result<ListModelsResponse> {
        self.get::<GetModels>().await
    }

    pub async fn send(&self, prompt: &str) -> anyhow::Result<String> {
        let res = self.exec_request(prompt, None).await?;

        let content = GrokClient::extract_response(&res)?;

        Ok(content)
    }

    /// Generate `n` images.
    ///
    /// The API constrains shape and size separately, and only by these two
    /// enums -- there is no way to ask for exact pixel dimensions.
    ///
    /// Requests base64 output so the bytes are returned inline in `b64_json`,
    /// avoiding a second download of a temporary URL.
    pub async fn generate_images(
        &self,
        prompt: &str,
        aspect_ratio: ImageAspectRatio,
        resolution: ImageResolution,
        n: u32,
    ) -> anyhow::Result<Vec<GeneratedImage>> {
        let req: GenerateImageRequest = GenerateImageRequest::builder()
            .model(Some(self.image_model.clone()))
            .prompt(Some(prompt.to_string()))
            .n(std::num::NonZeroU32::new(n))
            .aspect_ratio(Some(aspect_ratio))
            .resolution(Some(resolution))
            .response_format(Some("b64_json".to_string()))
            .try_into()
            .map_err(|e| anyhow::anyhow!("invalid image request: {e}"))?;

        Ok(self.post(&req).await?.data)
    }

    /// Generate a single image.
    pub async fn generate_image(
        &self,
        prompt: &str,
        aspect_ratio: ImageAspectRatio,
        resolution: ImageResolution,
    ) -> anyhow::Result<GeneratedImage> {
        let images = self
            .generate_images(prompt, aspect_ratio, resolution, 1)
            .await?;

        images.into_iter().next().context("no image returned")
    }

    /// Run a prompt and parse the response into a `serde` type `T`, using the
    /// type's JSON Schema to constrain the model's structured output.
    pub async fn eval_structured<T>(&self, prompt: &str) -> anyhow::Result<T>
    where
        T: serde::de::DeserializeOwned + schemars::JsonSchema,
    {
        let format = ModelResponseFormat::JsonSchema {
            description: None,
            name: Some(
                std::any::type_name::<T>()
                    .rsplit("::")
                    .next()
                    .unwrap_or("response")
                    .to_string(),
            ),
            schema: serde_json::to_value(schemars::schema_for!(T))?,
            strict: Some(true),
        };

        let res = self.exec_request(prompt, Some(format)).await?;

        let content = GrokClient::extract_response(&res)?;

        let parsed = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse structured response: {content}"))?;

        Ok(parsed)
    }

    //

    /// The enabled server-side search tools, or `None` when both are off so
    /// the field is left out of the request entirely.
    fn tools(&self) -> Option<Vec<ModelTool>> {
        let tools: Vec<ModelTool> = [
            self.web_search.then(web_search),
            self.x_search.then(x_search),
        ]
        .into_iter()
        .flatten()
        .collect();

        (!tools.is_empty()).then_some(tools)
    }

    fn extract_response(res: &ModelResponse) -> anyhow::Result<String> {
        let message = res
            .output
            .iter()
            .find_map(|item| match item {
                ModelOutput::OutputMessage(message) => Some(message),
                _ => None,
            })
            .context("no message")?;

        match message.content.first().context("no content")? {
            OutputMessageContent::OutputText { text, .. } => Ok(text.clone()),
            OutputMessageContent::Refusal { refusal } => anyhow::bail!("model refused: {refusal}"),
        }
    }

    async fn exec_request(
        &self,
        prompt: &str,
        format: Option<ModelResponseFormat>,
    ) -> anyhow::Result<ModelResponse> {
        let req: ModelRequest = ModelRequest::builder()
            .model(Some(self.model.clone()))
            .input(ModelInput::Array(vec![ModelInputPart::Object {
                content: ModelInputContent::String(prompt.to_string()),
                name: None,
                role: "user".to_string(),
                type_: None,
            }]))
            .tools(self.tools())
            .text(format.map(|format| ModelResponseConfiguration {
                format: Some(format),
            }))
            //.temperature(Some(0.7))
            .stream(Some(false))
            .try_into()
            .map_err(|e| anyhow::anyhow!("invalid request: {e}"))?;

        self.post(&req).await
    }

    /// POST a request body to the path the spec pairs it with, and decode the
    /// response type the spec pairs with that.
    async fn post<R: PostEndpoint>(&self, req: &R) -> anyhow::Result<R::Response> {
        let res = self
            .client
            .post(format!("{BASE_URL}{}", R::PATH))
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .json(req)
            .send()
            .await?;

        Self::decode::<R::Response>(res, R::PATH).await
    }

    /// GET the path a marker type names, and decode its response type.
    async fn get<E: GetEndpoint>(&self) -> anyhow::Result<E::Response> {
        let res = self
            .client
            .get(format!("{BASE_URL}{}", E::PATH))
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .send()
            .await?;

        Self::decode::<E::Response>(res, E::PATH).await
    }

    async fn decode<T: serde::de::DeserializeOwned>(
        res: reqwest::Response,
        path: &str,
    ) -> anyhow::Result<T> {
        let status = res.status();
        let text = res.text().await?;

        if !status.is_success() {
            anyhow::bail!("{path} failed ({status}): {text}");
        }

        serde_json::from_str(&text)
            .with_context(|| format!("unexpected response from {path}: {text}"))
    }
}
