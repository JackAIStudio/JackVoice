// End-to-end ASR probe using explicit development environment variables.
//
// Usage:
//   cargo run --example volc_probe -- /path/to/audio.pcm [hotword1,hotword2]
use jackvoice_lib::asr::{RealtimeSession, TranscriptUpdate, VolcAsrConfig};
use std::time::Duration;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let pcm_path = args
        .next()
        .unwrap_or_else(|| "/tmp/volc_probe.pcm".to_string());
    let hotwords_arg = args.next().unwrap_or_default();
    let hotwords: Vec<String> = if hotwords_arg.trim().is_empty() {
        Vec::new()
    } else {
        hotwords_arg
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    println!("[probe] engine: 火山引擎 豆包流式语音识别模型 2.0");

    let volc_key = load_api_key();
    let volc_config = VolcAsrConfig {
        api_key: volc_key,
        resource_id: std::env::var("JACKVOICE_VOLC_RESOURCE_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "volc.seedasr.sauc.duration".to_string()),
        boosting_table_id: std::env::var("JACKVOICE_VOLC_BOOSTING_TABLE_ID")
            .ok()
            .unwrap_or_default(),
    };

    let pcm = std::fs::read(&pcm_path).unwrap_or_else(|e| panic!("read pcm: {e}"));
    println!(
        "[probe] pcm bytes={} (~{:.2}s)",
        pcm.len(),
        pcm.len() as f32 / 32000.0
    );

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TranscriptUpdate>();
    let session = RealtimeSession::connect(
        volc_config,
        false,
        false,
        1300,
        hotwords,
        move |u: TranscriptUpdate| {
            let _ = tx.send(u);
        },
    )
    .await
    .expect("connect failed");
    println!("[probe] connected + session configured");

    // Printer task: show live partial/final updates; exits when the reader
    // task drops the sender after the session terminates.
    let printer = tokio::spawn(async move {
        let mut last = String::new();
        while let Some(u) = rx.recv().await {
            if u.text != last {
                println!("[update] final={} :: {}", u.is_final_sentence, u.text);
                last = u.text;
            }
        }
    });

    // Stream audio roughly in real time (100ms chunks).
    let mut i = 0usize;
    while i < pcm.len() {
        let end = (i + 3200).min(pcm.len());
        session
            .send_audio(pcm[i..end].to_vec())
            .await
            .expect("send_audio");
        i = end;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("[probe] all audio sent, finishing...");

    let final_text = session.finish().await.expect("finish");
    println!("[probe] FINAL: {final_text}");

    let _ = tokio::time::timeout(Duration::from_secs(3), printer).await;
}

fn load_api_key() -> String {
    std::env::var("JACKVOICE_VOLC_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .expect("set JACKVOICE_VOLC_API_KEY before running the development probe")
}
