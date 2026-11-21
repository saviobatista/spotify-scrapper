//! spotify-dl: baixa o video de um embedding do Spotify.
//!
//! Pipeline (sem navegador, usando a API publica do proprio Spotify):
//!   1. GET da page embed  -> extrai o `manifestId` do `__NEXT_DATA__`
//!   2. GET apresolve      -> resolve o host real do spclient (webgate)
//!   3. GET do manifesto   -> templates de init/segmento, perfis, token
//!   4. Para o perfil escolhido: baixa init + segmentos (0..N) e concatena
//!
//! O token `fauth` ja vem assinado dentro do manifesto (RS256 pelo signer do
//! CDN do Spotify) e serve para TODOS os segmentos da mesma encoding, entao
//! nenhum signing adicional e nenhuma credencial sao necessarios.

use std::env;
use std::fs::File;
use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

const UA: &str = "Mozilla/5.0 (compatible; spotify-dl/0.1)";
const EMBED_BASE: &str = "https://open.spotify.com/embed/episode/";

// ---------------------------------------------------------------------------
// Opções da linha de comando
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Opts {
    url: String,
    profile: Option<u32>,
    out: Option<String>,
    list_profiles: bool,
    subs: bool,
    mux_audio: bool,
    max_segments: Option<u64>,
    retries: u32,
}

const BANNER: &str = "spotify-dl  (video de embed do Spotify via API publica)";

fn usage() -> String {
    let mut s = format!("{BANNER}\n\nuso:\n  spotify-dl <url-embed> [opcoes]\n\nexemplos:\n  spotify-dl \"https://open.spotify.com/embed/episode/ID/video\"\n  spotify-dl <id> --profile 1 --out ep.mp4 --sub");
    s.push_str(
        "\n\nopcoes:\n  --profile <n>   perfil de video (0=1080p,1=720p,...). default: melhor disponivel\n  --out <arq>     arquivo de saida (mp4). default: <episodio>.mp4\n  --mux-audio     muxa a faixa de audio (aac) no mesmo mp4 (precisa do ffmpeg)\n  --list-perfis   lista os perfis disponiveis e sai\n  --sub           baixa tambem a legenda (.webvtt) no idioma disponivel\n  --max-segments  limita o numero de segmentos (debug)\n  --retries <n>   tentativas por requicao (default 5)",
    );
    s
}

fn parse_args() -> Result<Opts, String> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut opts = Opts {
        url: String::new(),
        profile: None,
        out: None,
        list_profiles: false,
        subs: false,
        mux_audio: false,
        max_segments: None,
        retries: 5,
    };

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" | "-o" => {
                i += 1;
                opts.out = Some(arg_value(&args, &mut i, "--out")?);
            }
            "--profile" => {
                i += 1;
                opts.profile = Some(
                    arg_value(&args, &mut i, "--profile")?
                        .parse()
                        .map_err(|_| "perfil invaldo".to_string())?,
                );
            }
            "--sub" => opts.subs = true,
            "--mux-audio" => opts.mux_audio = true,
            "--list-perfis" | "-l" => opts.list_profiles = true,
            "--max-segments" => {
                i += 1;
                opts.max_segments = Some(
                    arg_value(&args, &mut i, "--max-segments")?
                        .parse()
                        .map_err(|_| "max-segments invaldo".to_string())?,
                );
            }
            "--retries" => {
                i += 1;
                opts.retries = arg_value(&args, &mut i, "--retries")?
                    .parse()
                    .map_err(|_| "retries invaldo".to_string())?;
            }
            "-h" | "--help" => return Err(format!("{BANNER}\n\n{}", usage())),
            other => {
                if other.starts_with('-') {
                    return Err(format!("opcao desconhecida: {other}"));
                }
                if opts.url.is_empty() {
                    opts.url = other.to_string();
                } else {
                    return Err(format!("url ja fornecida: {}", opts.url));
                }
            }
        }
        i += 1;
    }

    if opts.url.is_empty() {
        return Err(format!("url ausente.\n\n{}", usage()));
    }
    Ok(opts)
}

fn arg_value(args: &[String], i: &mut usize, what: &str) -> Result<String, String> {
    if *i >= args.len() {
        return Err(format!("{what} exige um valor"));
    }
    Ok(args[*i].clone())
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// Faz GET em `url` com retentativas exponciais simples.
fn http_get(url: &str, retries: u32) -> Result<Vec<u8>, String> {
    let mut backoff = 1u32;
    for attempt in 0..=retries {
        match ureq::get(url).set("User-Agent", UA).call() {
            Ok(resp) => {
                let status = resp.status();
                if status >= 200 && status < 300 {
                    let mut body = Vec::new();
                    resp.into_reader()
                        .read_to_end(&mut body)
                        .map_err(|e| format!("erro ao ler resposta de {url}: {e}"))?;
                    return Ok(body);
                }
                return Err(format!("HTTP {status} de {url}"));
            }
            Err(_) if attempt < retries => {
                std::thread::sleep(Duration::from_secs(backoff.min(8) as u64));
                backoff *= 2;
            }
            Err(e) => return Err(format!("falha ao acessar {url}: {e}")),
        }
    }
    Err(format!("esgotadas as tentativas para {url}"))
}

fn http_get_str(url: &str, retries: u32) -> Result<String, String> {
    let bytes = http_get(url, retries)?;
    String::from_utf8(bytes).map_err(|_| format!("{url} nao e UTF-8 valido"))
}

// ---------------------------------------------------------------------------
// Etapa 1: extrair o manifestId da page embed
// ---------------------------------------------------------------------------

/// Converte qualquer URL de embed (episode/series, com ou sem /video) em uma
/// URL de page embed que carrega o `__NEXT_DATA__`.
fn embed_page_url(url: &str) -> String {
    let u = url.trim();
    if u.contains("open.spotify.com/embed/") {
        return u.to_string();
    }
    let id = extract_entity_id(u).unwrap_or_else(|| u.to_string());
    format!("{EMBED_BASE}{id}/video")
}

/// Tira o id de entidade de uma URL/embed.
fn extract_entity_id(url: &str) -> Option<String> {
    let u = url.trim();
    if let Some(rest) = u.strip_prefix(EMBED_BASE) {
        return Some(rest.split('/').next().unwrap_or("").to_string());
    }
    let parts: Vec<&str> = u.split('/').filter(|p| !p.is_empty()).collect();
    if let Some(pos) = parts.iter().position(|p| matches!(*p, "episode" | "series" | "show")) {
        if pos + 1 < parts.len() {
            return Some(parts[pos + 1].to_string());
        }
    }
    if parts.len() == 1 {
        return Some(parts[0].to_string());
    }
    None
}

/// Busca recursivamente o primeiro `"manifestId"` presente no JSON.
fn find_manifest_id(v: &Value) -> Option<String> {
    match v {
        Value::String(_) => None,
        Value::Array(a) => a.iter().find_map(find_manifest_id),
        Value::Object(m) => {
            if let Some(mid) = m.get("manifestId").and_then(Value::as_str) {
                return Some(mid.to_string());
            }
            m.values().find_map(find_manifest_id)
        }
        _ => None,
    }
}

fn extract_manifest_id(page_html: &str) -> Result<String, String> {
    let next = page_html
        .find("__NEXT_DATA__")
        .ok_or("page embed sem bloco __NEXT_DATA__ (page pode ter mudado)")?;
    let start = page_html[next..]
        .find('>')
        .map(|i| next + i + 1)
        .ok_or("malformado __NEXT_DATA__")?;
    let end = page_html[start..]
        .find("</script>")
        .map(|i| start + i)
        .ok_or("script de dados sem fechamento")?;
    let json_str = &page_html[start..end];
    let parsed: Value = serde_json::from_str(json_str)
        .map_err(|e| format!("nao parseou __NEXT_DATA__: {e}"))?;
    find_manifest_id(&parsed).ok_or_else(|| "nenhum manifestId encontrado na page".to_string())
}

// ---------------------------------------------------------------------------
// Etapa 2: resolver o host do spclient via apresolve
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Apresolve {
    spclient: Vec<String>,
}

fn resolve_spclient_host() -> Result<String, String> {
    let body = http_get_str("https://apresolve.spotify.com/?type=spclient", 5)?;
    let parsed: Apresolve = serde_json::from_str(&body).map_err(|e| format!("nao parseou apresolve: {e}"))?;
    parsed
        .spclient
        .first()
        .ok_or("apresolve nao devolveu host spclient")?
        .split(':')
        .next()
        .map(str::to_string)
        .ok_or("host spclient vazio".to_string())
}

// ---------------------------------------------------------------------------
// Etapa 3: baixar e parsear o manifesto
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Manifest {
    contents: Vec<Content>,
    initialization_template: String,
    segment_template: String,
    #[serde(default)]
    subtitle_template: Option<String>,
    #[serde(default)]
    subtitle_language_codes: Vec<String>,
    base_urls: Vec<String>,
    #[serde(default)]
    subtitle_base_urls: Vec<String>,
}

#[derive(Deserialize, Clone)]
struct Content {
    encoding_id: String,
    segment_length: u64,
    end_time_millis: u64,
    profiles: Vec<Profile>,
}

#[derive(Deserialize, Clone)]
struct Profile {
    id: u32,
    file_type: String,
    #[serde(default)]
    video_bitrate: u64,
    #[serde(default)]
    video_width: Option<u32>,
    #[serde(default)]
    video_height: Option<u32>,
    #[serde(default)]
    audio_bitrate: u64,
}

impl Profile {
    fn is_video(&self) -> bool {
        self.file_type == "mp4" && self.video_bitrate > 0
    }
}

fn fetch_manifest(host: &str, manifest_id: &str) -> Result<Manifest, String> {
    let url = format!(
        "https://{host}/manifests/v9/unauth/json/sources/{manifest_id}/options/supports_drm"
    );
    let body = http_get_str(&url, 5)?;
    serde_json::from_str(&body).map_err(|e| format!("nao parseou manifesto: {e}"))
}

// ---------------------------------------------------------------------------
// Etapa 4: URL builder + contagem de segmentos + download
// ---------------------------------------------------------------------------

/// Monta a URL de um segmento (ou init) a partir do template do manifesto.
fn build_url(base: &str, template: &str, profile: u32, segment: Option<u64>, file_type: &str) -> String {
    let t = template
        .replace("{{profile_id}}", &profile.to_string())
        .replace("{{file_type}}", file_type);
    let path = match segment {
        Some(n) => t.replace("{{segment_timestamp}}", &n.to_string()),
        None => t.replace("{{segment_timestamp}}", "inits"),
    };
    format!("{base}{path}")
}

/// Busca binaria: encontra o maior indice de segmento que responde 200.
/// Retorna o numero real de segmentos (idx 0..count-1).
fn count_segments(
    base: &str,
    template: &str,
    profile: u32,
    file_type: &str,
    step: u64,
    end_time_millis: u64,
) -> Result<u64, String> {
    if step == 0 {
        return Err("segment_length do manifesto e zero".to_string());
    }
    // helper: converte um indice no timestamp (multiplo de step) usado na URL.
    let seg = |idx: u64| segment_ok(base, template, profile, file_type, idx.saturating_mul(step));
    if !seg(0) {
        return Err("segmento 0 nao existe para este perfil (talvez audio/video incompativel)".to_string());
    }
    // O manifesto traz o tempo total; o numero de segmentos e ~end_time/step.
    // O ultimo indice valido fica proximo de `est`. Busca local ao redor dela.
    let est = end_time_millis / 1000 / step;
    // subir a partir de `est` ate o primeiro 404 (cobrindo a margem do fim)
    let mut last_idx = 0u64;
    let mut idx = est.max(1);
    while idx <= est + 64 {
        if seg(idx) {
            last_idx = idx;
            idx += 1;
        } else {
            break;
        }
    }
    // se `est` ja passava do fim, desce ate o primeiro 200
    if last_idx == 0 {
        let mut idx = est;
        while idx >= 1 {
            idx -= 1;
            if seg(idx) {
                last_idx = idx;
                break;
            }
        }
    }
    if last_idx == 0 {
        return Err("nenhum segmento valido perto do fim".to_string());
    }
    Ok(last_idx + 1)
}

fn segment_ok(base: &str, template: &str, profile: u32, file_type: &str, n: u64) -> bool {
    let url = build_url(base, template, profile, Some(n), file_type);
    match ureq::get(&url)
        .set("User-Agent", UA)
        .timeout(Duration::from_secs(30))
        .call()
    {
        Ok(resp) => resp.status() == 200,
        Err(_) => false,
    }
}

/// Baixa init + segmentos na ordem e concatena em `out`. `limit` limita o
/// numero de segmentos baixados (<= count); usado para testes rapidos.
/// O timestamp de cada segmento e `k * step`.
fn download_video(
    base: &str,
    init_tmpl: &str,
    seg_tmpl: &str,
    profile: u32,
    file_type: &str,
    count: u64,
    step: u64,
    limit: u64,
    out: &str,
) -> Result<u64, String> {
    let mut f = File::create(out).map_err(|e| format!("nao criou {out}: {e}"))?;
    let mut written = 0u64;

    let init_url = build_url(base, init_tmpl, profile, None, file_type);
    let init_bytes = http_get(&init_url, 5)?;
    f.write_all(&init_bytes).map_err(|e| format!("nao escreveu init: {e}"))?;
    written += init_bytes.len() as u64;

    let n_end = count.min(limit);
    for n in 0..n_end {
        if n % 128 == 0 || n == n_end - 1 {
            eprintln!("  segmento {}/{}  ({} bytes até agora)", n + 1, n_end, written);
        }
        let ts = n.saturating_mul(step);
        let url = build_url(base, seg_tmpl, profile, Some(ts), file_type);
        let bytes = http_get(&url, 5)?;
        f.write_all(&bytes).map_err(|e| format!("nao escreveu segmento {n} (ts {ts}): {e}"))?;
        written += bytes.len() as u64;
    }
    Ok(written)
}

// ---------------------------------------------------------------------------
// Audio (mux via ffmpeg)
// ---------------------------------------------------------------------------

/// Encontra o primeiro perfil de audio (mp4/aac) do mesmo content.
fn find_audio_profile(content: &Content) -> Option<&Profile> {
    content.profiles.iter().find(|p| p.file_type == "mp4" && p.audio_bitrate > 0 && p.video_bitrate == 0)
}

/// Junta video e audio em um unico mp4 sem recodificar (precisa do ffmpeg).
fn mux_with_ffmpeg(video: &str, audio: &str, out: &str) -> Result<(), String> {
    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-i", video, "-i", audio, "-map", "0:v:0", "-map", "1:a:0", "-c", "copy", out])
        .output()
        .map_err(|e| format!("nao encontrou o ffmpeg: {e}"))?;
    if status.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&status.stderr);
        Err(format!("mux falhou (ffmpeg):\n{err}"))
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(s) => {
            eprintln!("{s}");
            return ExitCode::from(2);
        }
    };

    // Etapa 1: page embed -> manifestId
    let page_url = embed_page_url(&opts.url);
    eprintln!("[1/5] page embed: {page_url}");
    let html = match http_get_str(&page_url, opts.retries) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("ERRO: {e}");
            return ExitCode::FAILURE;
        }
    };
    let manifest_id = match extract_manifest_id(&html) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("ERRO: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("      manifestId: {manifest_id}");

    // Etapa 2: apresolve -> host
    let host = match resolve_spclient_host() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("ERRO: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("[2/5] spclient host: {host}");

    // Etapa 3: manifesto
    let manifest = match fetch_manifest(&host, &manifest_id) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("ERRO: {e}");
            return ExitCode::FAILURE;
        }
    };
    let content = manifest.contents.first().cloned().unwrap_or_else(|| Content {
        encoding_id: String::new(),
        segment_length: 0,
        end_time_millis: 0,
        profiles: Vec::new(),
    });

    let video_profiles: Vec<&Profile> = content.profiles.iter().filter(|p| p.is_video()).collect();
    if video_profiles.is_empty() {
        eprintln!("ERRO: manifesto sem perfis de video mp4");
        return ExitCode::FAILURE;
    }

    if opts.list_profiles {
        println!("encoding:   {}", content.encoding_id);
        println!("duracao:    {} ms (~{} s)", content.end_time_millis, content.end_time_millis / 1000);
        println!("perfis de video disponiveis:");
        for p in &video_profiles {
            println!(
                "  profile {:>2}  {}x{}  video~{} kb/s  mp4",
                p.id,
                p.video_width.unwrap_or(0),
                p.video_height.unwrap_or(0),
                p.video_bitrate / 1000
            );
        }
        if let Some(langs) = manifest.subtitle_language_codes.first() {
            println!("subtitulos: {langs} (template disponivel)");
        }
        return ExitCode::SUCCESS;
    }

    // escolhe o perfil
    let profile = match opts.profile {
        Some(p) => {
            if video_profiles.iter().any(|x| x.id == p) {
                p
            } else {
                eprintln!("ERRO: perfil {p} inexistente. Use --list-perfis para ver as opções.");
                return ExitCode::FAILURE;
            }
        }
        None => video_profiles.last().unwrap().id, // melhor qualidade
    };
    let profile_info = video_profiles.iter().find(|p| p.id == profile).unwrap();
    let file_type = "mp4";

    // saída padrão
    let out_dir = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let out = match &opts.out {
        Some(o) => o.clone(),
        None => {
            let id = extract_entity_id(&opts.url).unwrap_or_else(|| "episode".to_string());
            format!("{}/{}.mp4", out_dir.display(), id)
        }
    };

    // escolhe base_url (primeira que funciona)
    let base = manifest.base_urls.first().cloned().unwrap_or_default();
    eprintln!("[3/5] manifesto OK. encoding={} base={}", content.encoding_id, base);
    eprintln!(
        "      perfil={profile}  {}x{}  ~{} kb/s  (mp4)",
        profile_info.video_width.unwrap_or(0),
        profile_info.video_height.unwrap_or(0),
        profile_info.video_bitrate / 1000
    );

    // Etapa 4: conta segmentos e baixa
    let step = content.segment_length;
    eprintln!("[4/5] contagem de segmentos...");
    let count = match count_segments(
        &base,
        &manifest.segment_template,
        profile,
        file_type,
        step,
        content.end_time_millis,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ERRO: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("      {count} segmentos (idx 0..{})", count.saturating_sub(1));

    let limit = opts.max_segments.unwrap_or(count);
    eprintln!("[5/5] baixando e concatenando -> {out}");
    match download_video(
        &base,
        &manifest.initialization_template,
        &manifest.segment_template,
        profile,
        file_type,
        count,
        step,
        limit,
        &out,
    ) {
        Ok(total) => {
            let mb = total as f64 / (1024.0 * 1024.0);
            eprintln!("      pronto: {total} bytes (~{mb:.1} MB) em {out}");
        }
        Err(e) => {
            eprintln!("ERRO: {e}");
            return ExitCode::FAILURE;
        }
    }

    // Etapa 5: audio (opcional, via ffmpeg)
    if opts.mux_audio {
        let audio_profile = match find_audio_profile(&content) {
            Some(p) => p,
            None => {
                eprintln!("      (sem faixa de audio no manifesto; mantendo so video)");
                return ExitCode::SUCCESS;
            }
        };
        eprintln!("[5/5] baixando audio (profile {}) e muxando...", audio_profile.id);
        let audio_out = format!("{out}.audio.mp4");
        let audio_count = match count_segments(
            &base,
            &manifest.segment_template,
            audio_profile.id,
            "mp4",
            step,
            content.end_time_millis,
        ) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("      aviso audio: {e}");
                return ExitCode::FAILURE;
            }
        };
        let audio_limit = opts.max_segments.unwrap_or(audio_count);
        if download_video(
            &base,
            &manifest.initialization_template,
            &manifest.segment_template,
            audio_profile.id,
            "mp4",
            audio_count,
            step,
            audio_limit,
            &audio_out,
        )
        .is_err()
        {
            eprintln!("      aviso audio: nao baixou audio; mantendo so video");
            let _ = std::fs::remove_file(&audio_out);
            return ExitCode::SUCCESS;
        }
        let muxed = format!("{out}.mux.mp4");
        match mux_with_ffmpeg(&out, &audio_out, &muxed) {
            Ok(_) => {
                let _ = std::fs::rename(&muxed, &out);
                let _ = std::fs::remove_file(&audio_out);
                eprintln!("      audio muxado em: {out}");
            }
            Err(e) => eprintln!("      aviso mux: {e} (video salvo sem audio)"),
        }
    }

    // Etapa 6: legendas (opcional)
    if opts.subs {
        match &manifest.subtitle_template {
            Some(tmpl) => {
                let base = manifest
                    .subtitle_base_urls
                    .first()
                    .cloned()
                    .or_else(|| manifest.base_urls.first().cloned())
                    .unwrap_or_default();
                let url = format!("{base}{tmpl}");
                let path = format!("{}/subtitle.webvtt", out_dir.display());
                match http_get(&url, 5) {
                    Ok(bytes) => match std::fs::write(&path, &bytes) {
                        Ok(_) => eprintln!("      legenda salva em: {path}"),
                        Err(e) => eprintln!("      aviso subtitle: nao salvou {path}: {e}"),
                    },
                    Err(e) => eprintln!("      aviso subtitle: {e}"),
                }
            }
            None => eprintln!("      (sem subtitle no manifesto)"),
        }
    }

    ExitCode::SUCCESS
}
