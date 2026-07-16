use anyhow::{anyhow, Result};

/// ElevenLabs 텍스트→음성(mp3). `ELEVENLABS_API_KEY`가 있을 때만 활성화된다(없으면 None).
/// 기본 음성은 Rachel(미국식 여성), 기본 모델은 flash v2.5(저렴). 둘 다 env로 교체 가능.
#[derive(Clone)]
pub struct Tts {
    api_key: String,
    voice_id: String,
    model: String,
    http: reqwest::Client,
}

impl Tts {
    /// env에서 설정을 읽는다. 키가 없거나 비어 있으면 None(기능 비활성).
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("ELEVENLABS_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty())?;
        // Rachel(미국식 여성). 다른 음성을 쓰려면 ELEVENLABS_VOICE_ID로 교체.
        let voice_id =
            std::env::var("ELEVENLABS_VOICE_ID").unwrap_or_else(|_| "21m00Tcm4TlvDq8ikWAM".into());
        // flash v2.5: 0.5크레딧/글자로 저렴·빠름. 최고 품질은 eleven_multilingual_v2.
        let model =
            std::env::var("ELEVENLABS_MODEL").unwrap_or_else(|_| "eleven_flash_v2_5".into());
        Some(Self {
            api_key,
            voice_id,
            model,
            http: reqwest::Client::new(),
        })
    }

    /// 캐시 파일명에 넣을 음성·모델 태그(같은 본문이라도 음성/모델이 다르면 별도 캐시).
    pub fn cache_tag(&self) -> String {
        format!("{}-{}", self.voice_id, self.model)
    }

    /// 텍스트를 mp3 바이트로 합성한다.
    pub async fn synthesize(&self, text: &str) -> Result<Vec<u8>> {
        let url = format!(
            "https://api.elevenlabs.io/v1/text-to-speech/{}",
            self.voice_id
        );
        let body = serde_json::json!({
            "text": text,
            "model_id": self.model,
            "voice_settings": { "stability": 0.5, "similarity_boost": 0.75 }
        });
        let resp = self
            .http
            .post(&url)
            .header("xi-api-key", &self.api_key)
            .header("accept", "audio/mpeg")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let txt = resp.text().await.unwrap_or_default();
            return Err(anyhow!("ElevenLabs error {status}: {txt}"));
        }
        Ok(resp.bytes().await?.to_vec())
    }
}
