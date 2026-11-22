# 共享 follow-tail 的真实消费者

日期：2026-11-22。基线：`f3fa6f4d8bf22cfd2baf490d9c3a08bd39af4d42`。范围是现有 React Inspect / IM 阅读表面，与 [Proto-UI #746](https://github.com/Proto-UI/Proto-UI/issues/746) 协作；不创建替代 App，也不改主体、账本或模型行为。

## 修改与责任

两个表面现在消费公开 Shadcn MessageScroller。Proto-UI 的 Scroll Module / Host 拥有跟随、用户离开、自然到达、合并布局、暂停和 lease 清理；Shenko 只观察 `following` 并请求 `pauseFollowing()` / `jumpToEnd()`。原先两个局部像素门限和自动写入末尾位置的代码已移除。

Shenko 继续拥有稳定 message/part key、偏移、选区、持久阅读状态与主体身份。窄组合关闭通用 anchor compensation，保留一个稳定 key 恢复者。Adapter setup 完成后的父 layout effect 才公布引用。重开时保留原始持久 key，通过 HTTP 历史、WS replay 和 late compiled children 的布局完成恢复；用户新输入接管后退休这个恢复提示。旧主体的订阅、observer、timer 和 frame 不能修改新 owner，renderer 的重复 dispose 和迟到 patch 也不会重建页面。键盘激活恢复按钮后保留其焦点。

构建显式使用 Shenko tsconfig，避免链接上游仓库时混合开发 `core/src` 与发布 `core/dist`。浏览器依赖图拒绝上游 `packages/**/src`；私有 compiler 仍仅在构建期生成自己的 message-bubble / transport-badge。所有当前来源、版本与资源摘要在 `web/upstream-pin.json` 和 `src/static/build-manifest.json`。

## 已执行的候选

候选 v5 使用 Proto-UI `d5153ea45530446aa01288c3cd4d2c346c381920`。实际生产 `index.html` / bundle / live controller / React renderer 运行，HTTP 和 WebSocket 提供明确标注的确定性读事实。Chromium 145.0.7632.6、Playwright 1.58.2、Node 24.22.3，light、reduced-motion、900 px 高；Inspect / IM × 1280 / 390 共四条旅程全部通过，54 个检查为通过，零 page error。上游保留 [完整记录与原件](https://github.com/Proto-UI/Proto-UI/blob/work/s7-harness/internal/records/2026-11-22-sprint-7-harness.md)。该链接是协作来源分支，最终整合引用由配套 PR 固定。

检查覆盖真实 committed reply Markdown、追加后的输入焦点、原生浏览器 wheel 离开、同一 key 与 ≤2 px 偏移、重复 replay、Inspect 重开恢复、键盘回到底部、继续跟随、选区保留、自然回到底部、主体替换、旧 observer 回调、pagehide 清理及零业务写入。wheel / keyboard 来自浏览器驱动；选区是显式 DOM Range。没有声称物理键盘/IME、触屏硬件、辅助技术、所有主题、原生后端、部署或模型验收。执行环境缺少 emoji 字形，此旅程不接受完整字体或视觉同等性。

原失败分别保留：v1 过早订阅 exposes；v3 上游源码与 dist 混入造成 Hook context 分裂；v4 测试错误要求原生 `button` 标签、移动重开早到 fallback 覆盖 key。v4 的 submission-view fixture 还缺少 `schema_version: 2`，所以 IM 尚未正确关联回复；v5 修正事实前置，并直接检查真实回复文字，不能拿系统摘要替代消息消费者。

现有前端测试 51/51 通过，串行 `tsc --noEmit` 无诊断。一次在上游重建 dist 期间运行的类型检查失败被保留为构建顺序问题；之后等待构建结束再运行。静态内容断言改为渲染同一个生产 `Messages` 行组件，因为 Adapter viewport 在客户端才 materialize；完整 App 的行为由浏览器旅程验收。原断言的消息、分组与操作内容都保留。

最终配套 pin 和整合版本的完整公开包构建、独立评审与浏览器回执，须随实际 PR 更新；v5 的旧源码结果不会自动转为新整合 head 的通过。

## 重现入口

按 `web/upstream-pin.json` 检出 Proto-UI，在 Node 24 下使用其锁定 pnpm 安装，再在上游运行：

```sh
node scripts/build/public-packages.mjs --package @proto.ui/adapter-react --package @proto.ui/prototypes-shadcn --package @proto.ui/cli --json > /tmp/shenko-public-build.json
```

在 Shenko `crates/shenko-ui/web` 用 `corepack pnpm@10.32.1 install --frozen-lockfile --ignore-scripts` 安装声明依赖。设置 `PROTO_UI_SOURCE` 为该精确 checkout、`PROTO_UI_BUILD_RECEIPT` 为上述完整回执，再依次运行 `node build.mjs`、`corepack pnpm@10.32.1 exec tsc --noEmit`、`node --test tests/*.test.mjs`。不要与重建上游 dist 并发类型检查。

使用已安装的 Playwright 运行仓库根目录的 `scripts/web_reading_journey.mjs`。`PLAYWRIGHT_MODULE` 可指向现有 `playwright-core/index.mjs`，`CHROMIUM_EXECUTABLE` 可绑定已有浏览器，`READING_EVIDENCE_DIR` 指定独立输出目录。脚本逐格保留 receipt 和截图，失败不会跳过其余格；脚本只为真实前端提供读 fixture，不连接实际主体或向后端发送业务操作。

Rust 二进制嵌入资源的重编译、服务实际部署和完整 Shenko S1–S5 条件仍保留。本轮浏览器直接运行新 bundle，不把旧二进制或 manifest 修改当作已部署升级。
