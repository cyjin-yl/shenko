# 媒体能力功能合同 — SPEC §15.2、§20、§21、§35.3、§41.2.2/41.3、A16

本文解释媒体实现边界，不替换、删减或放宽 `SPEC` 的任何要求。转写文本只来自真实引擎；本仓库内的 runtime 适配器是 FunASR websocket 与 OpenAI 兼容 `/v1/audio/speech` 两个有文档协议的具体实现，不是空 recognizer 占位。

## 一个事件模型，同一主体

`MediaIngressEvent` 有五个变体：`StreamOpened`、`StreamState`、`Revision`、`Recording`、`Priority`。它们都以同一 `inbox_events` 原始身份进入同一主体收件箱（kind 分别为 `media_stream` / `media_revision` / `media_recording` / `media_priority`，source 均为 `media/<provider>`），由同一 pending drain 恢复，保留完整工具委派能力。打断、插话、停止播音与紧急消息是优先输入，不是第二个主 loop（SPEC §20）。

每次 admission 在一个账本事务里提交：原始事实行 + 最新有效观察 + 绑定收件箱原始件；活队列发送只是唤醒，丢失唤醒由 drain 以同一事件身份恢复。重放返回同一事件身份的存盘回执，不重复送达。

## 持久层闭包

| 表 | 内容 | 复制语义 |
| --- | --- | --- |
| `media_streams` / `media_stream_ends` / `media_segment_revisions` | 既有流与转写修订 | 既有规则不变 |
| `media_recordings` | recording 最新状态投影（无 `data` 列，一致性按列对 envelope 校验） | 恒等列不可变；状态只允许向终态迁移（`recording→stopped/failed`，时间戳单调） |
| `media_recording_controls` | 每次 start/stop 控制通知（`data` 为完整 envelope，含投影状态） | 完全不可变；投影行重绑其最新控制事件的身份 |
| `media_priority_inputs` | 优先输入原文 | 完全不可变 |

恢复导入时每个流闭包组（含三类新媒体表）在 savepoint 内整体应用；冲突保留本地完整绑定并记录冲突事实。`Sealed` 音频引用在元数据提交前验证 CAS 字节（digest+length）；provider-local handle 只逐字记录。

## 实时会话（FunASR websocket）

`FunAsrWebsocketProvider` 按 FunASR runtime 文档实现：连接服务端根路径后第一条消息是 JSON 控制帧 `{mode, chunk_size, chunk_interval, encoder_chunk_look_back, decoder_chunk_look_back, audio_fs, wav_name, wav_format:"pcm", is_speaking:true, hotwords, itn}`，随后是二进制 mono PCM16LE 帧（binding 的 sample rate），结束时 `{"is_speaking":false,"is_end":true}` 并等待服务端 `{"is_end":true,"is_final":true}` ack（30 秒超时为 typed 错误）。

服务端 JSON `{text, mode, is_final, is_end, wav_name, timestamp, spk_name, spk_score}` 中 `mode∈{online, 2pass-online, 2pass-offline, offline}`：`online` 结果是 partial，`2pass-offline/offline` 是 final。每个引擎事件经 runtime 的 sink 进入 `admit_revision`：partial 修订不带音频；segment-final 修订封上该段录音（见下）并推进段号。

**更正**：同一引擎对同段说话人 claim 变化时，runtime 把后续 claim 标 `corrected=true`；文本更正常规走 `FinalCorrection` disposition。迟到修订保持既有 `Superseded` 语义。

## 录音与音频 CAS 闭包

会话以 `recording: Some(MediaRecordingScope)` 打开时，runtime 先承认一条 `Start` 控制（durable），随后每个 push 的 PCM16 帧同时进入 `MediaAudioRecorder`。段终稿时录音器把该段 PCM 封装成 44 字节规范 WAVE 头并用 `seal_object(namespace="media-audio", codec="wav-pcm16", major=1)` 封口；`Sealed` 引用挂在该段 final 修订上，主体读到的就是提供方听到的字节。单段 PCM 上界 16 MiB（约 8.5 分钟 @16kHz），触界提前封口并在该修订 `provider_facts.recording_truncated=true` 如实标注，后续音频开始新段。

`Stop` 控制立即成为 durable 事实；打开中的段字节在下一条 final 修订上封口。会话在 final 修订到达前消亡时，尾部 PCM 丢弃并在日志记录——这是诚实的边界，不补造提供方没有说的文本。

## 优先输入与 barge-in

`MediaPriorityInput`：`BargeIn` / `Interjection` / `StopPlayback` / `UrgentMessage`，可携带提供方原文、时间与音频引用。`OpenAiSpeechOutput` 的播放是本地命令子进程（默认 `ffplay -nodisp -autoexit <cas-path>`，`player_command` 可换成 `aplay`/`paplay`，要求以音频路径为最后一个参数）；播放回执区分 `Finished` / `Stopped` / `Failed`。

远侧转写到达时若正在播音，runtime 先 `stop_playback()`（杀子进程）再把 `BargeIn` 作为优先输入承认——顺序保证主体先读到停止、再读到打断它的词。`StopPlayback` 优先输入和 `/api/media/playback/stop` 走同一停止路径。

## 语音合成（OpenAI 兼容）

`POST {endpoint}/v1/audio/speech`，body `{model, input, voice?, response_format, speed?}`，`Authorization: Bearer $<credential_env>`。非 2xx 是 typed 协议错误（`media_speech_rejected`，不自动重试）；响应字节按 response_format 记 media type 后封口（`namespace="media-speech", codec="speech-audio", major=1`）。binding 的 `credential_env` 只给变量名，秘密不进 binding/store/事实。

## 声纹与说话人 claim

FunASR 结果的 `spk_name`/`spk_score` 逐字映射为 `MediaSpeakerClaim{speaker, confidence, corrected, source:"funasr:<model>"}`：空或缺失的 `spk_name` 保持未知（不产生 claim）；confidence 是模型信号，不构成身份权威（§35.3）。`FunAsrSpeakerIdentityInCas::identify` 对一份已封口 WAV 开一条 offline 短会话取引擎 claim。person 目录属 Contacts 层：关联经 `admit_person_claim(SpeakerObservation, Candidate, …)`，媒体层不写 person 存储，`person_id` 由 Contacts 侧回填。

## 读取面

- 原生 child `media.read` v1（kind `media.read`）：frozen+sealed 输入、op=`transcript|recording|priority`、只读本主体、观察封口为证据、重放经原 armed attempt 调和；永不返回音频字节。
- UI：`/api/media/ingress/*`（既有外部提供方入口）、`/api/media/session*`（开关会话、推音频、状态）、`/api/media/recording`（控制+查询）、`/api/media/priority`、`/api/media/speech?play=`、`/api/media/playback/stop`。未接线时返回 `available:false`，不伪造 admission。

## 配置

`<data_dir>/media/providers.json` 为 `MediaProviderBinding` 数组：`{provider, account, capability: realtime_session|speech_output|speaker_identity, endpoint, credential_env?, model?, sample_rate, options}`。文件缺失=未配置引擎（外部提供方 ingress 路径仍可用）；条目畸形是 typed 错误。runtime 按 capability 装配三个适配器。

## 外部前提与验证

本仓库不含 ASR/TTS 引擎二进制。真实运行需要：FunASR websocket 服务端（如 `funasr_wss_server.py`，加载 ASR 模型，可选 CAM++/SV 声纹模型）与任一 OpenAI 兼容 TTS 端点；播放需要 `ffplay` 或 `aplay`。父级串行验证：引擎起好后 (1) `PUT /api/media/session` 开会话并推 PCM16；(2) 检查 `media_segment_revisions`/`media_events` 收件箱原始件与 pending drain 恢复；(3) 播音中注入远侧转写，断言 `media_priority_inputs` 有 `barge_in` 且播放回执为 `Stopped`；(4) 录音段封口后按 digest 读回 WAV 字节与提供方输入一致；(5) 复制导入含三类新媒体表的档案，断言闭包组一致落库。

## 验收映射

- A16 通话与旁听：同一主体接受音频事件；录音状态经控制通知真实可见；委派链不变；barge-in 与转写修订不重复动作。
- §20：录音状态/存储范围/停止控制真实可见；优先输入同一事件模型；成果保留时间轴来源（`captured_at_ms`、provider_facts、WAVE 样本率）。
- §35.3：说话人 claim 保持未知/置信/更正语义，模型信号不升级为身份权威。
- §41.2.2：对象先字节封口、引用后查证；provider-local handle 不进持久音频身份。
