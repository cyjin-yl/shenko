# 媒体能力功能合同 — [§15.2](contacts.md#spec-15-2)、[§20](media.md#spec-20)、[§21](web-interface.md#spec-21)、[§35.3](plugins.md#spec-35-3)、[§41.2.2](durable-state.md#spec-41-2-2)、[§41.3](durable-state.md#spec-41-3)、A16

**内核定位。** 参考插件，建立在[内核合同](kernel.md)之上。媒体入站是原始事实（[内核 3](kernel.md#kernel-3)），转写与合成是带回执的效果（[内核 5](kernel.md#kernel-5)），处理器经[内核 7](kernel.md#kernel-7)登记。

本文解释媒体实现边界，不替换、删减或放宽 `SPEC` 的任何要求。转写文本只来自真实引擎；本仓库内的 runtime 适配器是 FunASR websocket 与 OpenAI 兼容 `/v1/audio/speech` 两个有文档协议的具体实现，不是空 recognizer 占位。

## 一个事件模型，同一主体

`MediaIngressEvent` 有五个变体：`StreamOpened`、`StreamState`、`Revision`、`Recording`、`Priority`。它们都以同一 `inbox_events` 原始身份进入同一主体收件箱（kind 分别为 `media_stream` / `media_revision` / `media_recording` / `media_priority`，source 均为 `media/<provider>`），由同一 pending drain 恢复，保留完整工具委派能力。打断、插话、停止播音与紧急消息是优先输入，不是第二个主 loop（[§20](media.md#spec-20)）。

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

FunASR 结果的 `spk_name`/`spk_score` 逐字映射为 `MediaSpeakerClaim{speaker, confidence, corrected, source:"funasr:<model>"}`：空或缺失的 `spk_name` 保持未知（不产生 claim）；confidence 是模型信号，不构成身份权威（[§35.3](plugins.md#spec-35-3)）。`FunAsrSpeakerIdentityInCas::identify` 对一份已封口 WAV 开一条 offline 短会话取引擎 claim。person 目录属 Contacts 层：关联经 `admit_person_claim(SpeakerObservation, Candidate, …)`，媒体层不写 person 存储，`person_id` 由 Contacts 侧回填。

## 读取面

- 原生 child `media.read` v1（kind `media.read`）：frozen+sealed 输入、op=`transcript|recording|priority`、只读本主体、观察封口为证据、重放经原 armed attempt 调和；永不返回音频字节。
- UI：`/api/media/ingress/*`（既有外部提供方入口）、`/api/media/session*`（开关会话、推音频、状态）、`/api/media/recording`（控制+查询）、`/api/media/priority`、`/api/media/speech?play=`、`/api/media/playback/stop`。未接线时返回 `available:false`，不伪造 admission。

## 配置

`<data_dir>/media/providers.json` 为 `MediaProviderBinding` 数组：`{provider, account, capability: realtime_session|speech_output|speaker_identity, endpoint, credential_env?, model?, sample_rate, options}`。文件缺失=未配置引擎（外部提供方 ingress 路径仍可用）；条目畸形是 typed 错误。runtime 按 capability 装配三个适配器。

## 外部前提与验证

本仓库不含 ASR/TTS 引擎二进制。真实运行需要：FunASR websocket 服务端（如 `funasr_wss_server.py`，加载 ASR 模型，可选 CAM++/SV 声纹模型）与任一 OpenAI 兼容 TTS 端点；播放需要 `ffplay` 或 `aplay`。父级串行验证：引擎起好后 (1) `PUT /api/media/session` 开会话并推 PCM16；(2) 检查 `media_segment_revisions`/`media_events` 收件箱原始件与 pending drain 恢复；(3) 播音中注入远侧转写，断言 `media_priority_inputs` 有 `barge_in` 且播放回执为 `Stopped`；(4) 录音段封口后按 digest 读回 WAV 字节与提供方输入一致；(5) 复制导入含三类新媒体表的档案，断言闭包组一致落库。

## 验收映射

- A16 通话与旁听：同一主体接受音频事件；录音状态经控制通知真实可见；委派链不变；barge-in 与转写修订不重复动作。
- [§20](media.md#spec-20)：录音状态/存储范围/停止控制真实可见；优先输入同一事件模型；成果保留时间轴来源（`captured_at_ms`、provider_facts、WAVE 样本率）。
- [§35.3](plugins.md#spec-35-3)：说话人 claim 保持未知/置信/更正语义，模型信号不升级为身份权威。
- [§41.2.2](durable-state.md#spec-41-2-2)：对象先字节封口、引用后查证；provider-local handle 不进持久音频身份。

<a id="spec-20"></a>
<!-- migrated-contract: SPEC:20 -->
# 20 实时通话与课堂旁听扩展

原生预留 RealtimeSessionProvider、MediaIngress、AudioOutput 和 SpeakerIdentity 等能力接口。这里的“预留槽位”是稳定插件扩展点，不代表不开通话时永久空占一个模型请求或 GPU slot。卸载媒体插件后，内核不应携带音视频框架、声纹模型或持续采样线程。

电话、局域网实时语音、课堂旁听通过插件接入；实际电话网络/WebRTC/SIP/聊天平台支持由 provider 说明，不假设所有 IM bot API 都能通话。一个实时会话仍使用同一 Agent 身份和主体序列；语音不是另起一条有自主目标的平行人格。

输入事件包含时间戳、片段序号、partial/final 修订、说话人候选、转写/原音频引用和会话 scope。VAD、ASR、声纹或原生多模态理解属于可替换处理器；partial transcript 的修订不能产生重复承诺，最终文本与原音频对应可追溯。连续音频可以聚合成适合主体处理的事件，不为每个音频包创建一个 LLM 子任务。

通话/旁听期间保留完整工具委派能力：主体可让孩子搜索概念、检查远程任务、整理课堂笔记、做 REPL 计算或保存记忆，收到结果后再决定何时说话。打断、插话、停止播音与紧急消息是同一事件模型中的优先输入；不能通过第二个主 loop 消除等待。

录音、声纹学习、讲者分离、实时翻译和课后整理均可由 Agent 按已授的媒体能力自主安排与发展。录音状态、正在存储的范围和停止控制必须真实可见；成果保留时间轴来源。Agent 可以委派开发/安装更好的媒体插件，在候选环境验证后提升，仍遵守包/权限/资源合同，不需要把未来所有媒体功能预先写死。

听课模式可选择安静旁听、只在被问时说话、主动补充；这是行为策略，主体可结合现场与用户偏好调整。ASR/TTS/声纹/视觉和 LLM 若共享设备，需要单独的容量与时延预算并由资源服务协调；通话前检查可实现的时延，不承诺一台只有单慢速推理槽的设备同时完成全部实时功能。

可采用低延迟语义事件与后台整理两条处理路径：提问、打断和明确呼叫尽快进入主体；长段内容由子 Agent 聚合索引和笔记。主体在课堂期间仍持续思考，并收到会话状态、适量内容事件及原文/原音频 handles，随时可查询过去内容或派工具。不能截留全部课堂输入，等固定三分钟才唤醒主体；静默旁听是少说话，不是停止思考。

回看窗口与摘要周期是可调参数，不以“30 秒/3 分钟”作普适规则。声纹分数是模型输出信号，不自动等于真实身份概率或 P0 资格。测量检测、排队、取消、推理、TTS 与播放全链路；提示音的毫秒延迟不当作语义回答的延迟。

<!-- /migrated-contract: SPEC:20 -->
