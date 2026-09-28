# Audio Normalizer

Aplicativo desktop (Tauri 2) que normaliza o volume de vídeos para um alvo de loudness (EBU R128 / ITU-R BS.1770), substituindo o próprio arquivo.

- **Front-end:** TypeScript + Vite (`src/`)
- **Processamento:** Rust (`src-tauri/src/engine/`): medição de loudness, ganho, limitador de true peak
- **Mídia:** bridge em C (`src-tauri/native/avbridge.c`) sobre libavformat/libavcodec/libswresample. Nenhum processo `ffmpeg` é executado.

## Como funciona

1. **Pré-análise** (`Analisar`): decodifica a 1ª faixa de áudio e mede loudness integrado, LRA, true peak e sample peak. Mostra o ganho necessário, o quanto o limitador vai atuar e um "potencial de melhoria". Trocar o alvo reavalia sem decodificar de novo. Os pacotes de áudio comprimidos ficam em memória (até 512 MiB) para a normalização não reler o vídeo.
2. **Normalização** (`Normalizar`):
   - aplica o ganho e um limitador *look-ahead* com detecção de true peak (oversampling 4x);
   - se a limitação prevista passar de 1 dB, calibra numa passada só (sem encode): cinco ganhos candidatos em paralelo e interpolação do que bate o alvo;
   - recodifica só a faixa de áudio normalizada, com o mesmo codec (AAC, Opus, MP3, Vorbis, AC-3, E-AC-3, FLAC, ALAC, PCM). Vídeo, legendas, anexos, capítulos, metadados e as outras faixas de áudio são copiados sem recompressão;
   - mede o resultado real decodificando os pacotes recém-codificados, sem reler o arquivo. Se o encoder AAC estourar o teto de pico, recodifica uma vez com `aac_coder=fast`;
   - confere duração e vídeo e então **substitui o original** (rename atômico no mesmo volume). Em erro ou cancelamento o original fica intacto e o temporário é apagado.

### Desempenho

Cada etapa pesada roda em pipeline com threads e filas limitadas: decode → ganho/limitador → encoder/muxer → medição do áudio codificado. Na análise, loudness e true peak (por canal) são medidos em paralelo. O arquivo de vídeo é lido uma vez na análise e uma vez na gravação. O limite prático é o encoder AAC nativo (~35 s para 30 min de áudio estéreo).

Alvos disponíveis: -14 (YouTube/Spotify/Instagram/TikTok), -16, -19, -23 (EBU R128), -24 LUFS (ATSC A/85). Teto: -1, -1,5 ou -2 dBTP. O ganho é limitado a ±24 dB.

## Requisitos

- Node 20+, Rust stable (MSVC no Windows), Visual Studio Build Tools (compilador C)
- **FFmpeg em versão "shared" com arquivos de desenvolvimento** (`include/`, `lib/`, `bin/`), ex.: `winget install BtbN.FFmpeg.LGPL.Shared.8.1`
- Variável `FFMPEG_DIR` apontando para essa pasta

O `build.rs` compila a bridge, faz o link com as libs e copia as DLLs do FFmpeg para a pasta do executável (dev) e para `src-tauri/runtime/` (empacotadas no instalador).

## Comandos

```bash
npm install
npm run app:dev      # abre o app em modo desenvolvimento
npm run app:build    # gera o instalador em src-tauri/target/release/bundle
npm run test:core    # testes unitários do engine (limitador, avaliação)
```

Teste ponta a ponta com arquivos reais (**os arquivos são modificados**, use cópias):

```bash
NORMALIZER_E2E_FILES="C:/tmp/a.mp4;C:/tmp/b.mkv" cargo test --manifest-path src-tauri/Cargo.toml e2e -- --ignored --nocapture
```

## Estrutura

```
src/                     UI (index.html, main.ts, api.ts, styles.css)
src-tauri/
  native/avbridge.{h,c}  bridge C: probe, decoder PCM, remuxer/encoder
  src/engine/
    av.rs                wrappers seguros da bridge (FFI)
    analyze.rs           medição EBU R128 + avaliação da melhoria
    limiter.rs           limitador true peak com look-ahead
    normalize.rs         pipeline de normalização e substituição do arquivo
    job.rs               progresso, cancelamento e utilidades de pipeline
    bench.rs             benchmarks (ignorados; usam BENCH_FILE)
  src/lib.rs             comandos Tauri
```
