# spotify-dl

Baixa o vídeo de um embedding de episódio/ série do Spotify para o disco local,
sem navegador. O vídeo vem como MP4 progressivo segmentado (fMP4, ISO 14496-12)
servido por `video-cf.spotifycdn.com`; a faixa de áudio vem em encoding
separado e é muxada de volta com o `ffmpeg`.

O fluxo inteiro usa apenas a API pública do Spotify (chamadas `GET` com `ureq`),
então não depende de Playwright nem de browser headless.

## Como funciona

1. Pega o `manifestId` do bloco `__NEXT_DATA__` da página embed.
2. Resolve o host `spclient` via `apresolve.spotify.com/?type=spclient`.
3. Busca o manifesto em
   `/manifests/v9/unauth/json/sources/{manifestId}/options/supports_drm`
   (sem autenticação). O manifesto traz os templates de URL com o token
   `fauth` já embutido, além dos perfis de vídeo e áudio disponíveis.
4. Conta os segmentos válidos (o passo é `segment_length = 4`; o timestamp de
   cada segmento é `indice * passo`) e baixa o vídeo.
5. Baixa a faixa de áudio e faz o mux com `ffmpeg` (`-c copy`), gerando um MP4
   único com vídeo + áudio. Legendas são opcionais (`--sub`).

> Os tokens `fauth`/`token_ak`/`token_cf` vêm do manifesto em tempo de execução;
> não são secrets estáticos e não ficam no código.

## Requisitos

- Rust (toolchain 1.75+).
- `ffmpeg` e `ffprobe` no `PATH` (só necessário para `--mux-audio`, o padrão).

## Instalar e compilar

```
cargo build --release
```

O binário fica em `target/release/spotify-dl`.

## Uso

```
./target/release/spotify-dl "https://open.spotify.com/embed/episode/3hpdPAOUmoYGJYbR98sTkn/video" --mux-audio --out episodio.mp4
```

Sem `--out`, o arquivo sai como `<id-do-entidade>.mp4` no diretório atual.

## Opções

| Opção | Descrição |
|-------|-----------|
| `--profile N` | Escolhe o perfil (resolução) de vídeo. O default é o maior disponível. |
| `--mux-audio` | Baixa a faixa de áudio e muxa com o vídeo (padrão quando não especificado o contrário). |
| `--sub` | Baixa e muxa a legenda (se houver) no container final. |
| `--out -o <arquivo>` | Nome do arquivo de saída. |
| `--list-perfis -l` | Lista os perfis de vídeo/ áudio disponíveis e sai. |
| `--max-segments N` | Para o download do vídeo após `N` segmentos (útil para testes). |
| `--retries N` | Número de tentativas por requisição (default 3). |

## Saída

MP4 ISO 14496-12 com:

- vídeo `h264` (perfil escolhido, ex. 320x180),
- áudio `aac` 96 kbps,
- duração coerente com o manifesto.

## Estrutura

- `src/main.rs` - pipeline completa: parse da página, resolução de host,
  download do manifesto, contagem de segmentos, download e mux.
- `Cargo.toml` - dependências: `ureq` (HTTP), `serde`/`serde_json` (JSON).

## Notas

- O User-Agent não é o fator decisivo: o CDN responde 200 para agentes genéricos.
- O download respeita a estrutura segmentada do manifesto; segmentos fora dos
  múltiplos de `segment_length` retornam 404 e não são baixados.
