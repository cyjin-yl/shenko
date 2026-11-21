"""Observe the existing real single-node journey through its actual Web UI.

This is not a second model scenario or a replay server. The page's composer
creates both new inputs; WebSocket listeners only observe the browser's real
traffic. The owning journey still controls the one CLI and checks its ledger.
"""
import asyncio
import hashlib
import importlib.metadata
import json
import platform
import time
from collections import Counter
from pathlib import Path


PLAYWRIGHT_VERSION = "1.62.0"
CONTROL_MESSAGES = {"hello", "capabilities", "submit", "query_submission", "receipt", "submission_view", "timeline_part"}


def save(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


class BrowserWitness:
    def __init__(self, evidence_dir, url):
        self.directory = evidence_dir / "browser"
        self.directory.mkdir()
        self.url = url
        self.log = (self.directory / "events.jsonl").open("w")
        self.started = time.monotonic()
        self.index = 0
        self.controls = []
        self.socket_count = 0
        self.changed = asyncio.Event()
        self.counts = Counter()
        self.page_errors = []
        self.committed_keys = set()
        self.submission_keys = set()
        self.captures = []
        self.checks = {}
        self.observations = {}
        self.driver = self.browser = self.context = self.page = None
        self.restart_index = None
        self.restart_socket_id = None
        self.reconnect_after = None
        self.restart_committed_keys = set()
        self.before_restart = None

    def record(self, kind, **data):
        self.index += 1
        row = {"index": self.index, "elapsed_seconds": time.monotonic() - self.started,
               "kind": kind, **data}
        self.log.write(json.dumps(row, ensure_ascii=False) + "\n")
        self.log.flush()
        self.changed.set()
        return row

    def frame(self, direction, socket_id, raw):
        if isinstance(raw, bytes):
            self.record("binary_frame", direction=direction, socket_id=socket_id,
                        bytes=len(raw), sha256=hashlib.sha256(raw).hexdigest())
            return
        try:
            message = json.loads(raw)
        except (ValueError, TypeError):
            self.record("invalid_json_frame", direction=direction, socket_id=socket_id, raw=raw)
            return
        row = self.record("frame", direction=direction, socket_id=socket_id, message=message)
        if isinstance(message, dict) and isinstance(message.get("type"), str):
            self.counts[(direction, message.get("type"))] += 1
            if message.get("type") in CONTROL_MESSAGES:
                self.controls.append(row)
            if message.get("type") == "timeline_part" and message.get("committed_only") is True:
                self.committed_keys.add("part." + json.dumps(
                    [message.get("message_id"), message.get("part_id")], ensure_ascii=False, separators=(",", ":")))

    def socket(self, socket):
        self.socket_count += 1
        socket_id = self.socket_count
        self.record("socket_created", socket_id=socket_id, url=socket.url)
        socket.on("framesent", lambda raw: self.frame("sent", socket_id, raw))
        socket.on("framereceived", lambda raw: self.frame("received", socket_id, raw))
        socket.on("socketerror", lambda error: self.record("socket_error", socket_id=socket_id, error=error))

        def closed(_socket):
            self.controls.append(self.record("socket_closed", socket_id=socket_id))
        socket.on("close", closed)

    async def wait_for(self, predicate, after=0, timeout=30):
        deadline = time.monotonic() + timeout
        while True:
            self.changed.clear()
            for row in self.controls:
                if row["index"] > after and predicate(row):
                    return row
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("expected browser wire observation did not arrive")
            await asyncio.wait_for(self.changed.wait(), remaining)

    def received(self, after=0):
        return [row["message"] for row in self.controls if row["index"] > after
                and row.get("direction") == "received"]

    def editor(self):
        return self.page.get_by_role("textbox", name="向同一主体追加输入", exact=True)

    async def open(self):
        from playwright.async_api import async_playwright

        installed = importlib.metadata.version("playwright")
        if installed != PLAYWRIGHT_VERSION:
            raise RuntimeError(f"browser witness requires playwright=={PLAYWRIGHT_VERSION}, got {installed}")
        self.driver = await async_playwright().start()
        # The package selects its matching Chromium build. The chromium
        # channel uses the real browser's headless mode, not a page mock.
        self.browser = await self.driver.chromium.launch(channel="chromium", headless=True)
        self.context = await self.browser.new_context(
            viewport={"width": 1280, "height": 900}, locale="zh-CN",
            color_scheme="light", reduced_motion="reduce")
        self.context.set_default_timeout(15000)
        self.page = await self.context.new_page()
        self.page.on("websocket", self.socket)

        def page_error(error):
            self.page_errors.append(str(error))
            self.record("page_error", error=str(error))
        self.page.on("pageerror", page_error)
        self.page.on("console", lambda message: self.record("console", level=message.type, text=message.text))
        self.page.on("requestfailed", lambda request: self.record(
            "request_failed", url=request.url, failure=request.failure))
        save(self.directory / "environment.json", {
            "playwright": installed, "browser": "Chromium", "browser_version": self.browser.version,
            "browser_executable_sha256": sha256(self.driver.chromium.executable_path),
            "platform": platform.platform(), "python": platform.python_version(),
            "headless": True, "channel": "chromium", "viewport": {"width": 1280, "height": 900},
            "locale": "zh-CN", "color_scheme": "light", "reduced_motion": "reduce", "url": self.url,
        })
        await self.page.goto(self.url, wait_until="domcontentloaded")
        await self.page.locator('.shell[data-source="live"]').wait_for(state="visible")
        await self.editor().wait_for(state="visible")
        await self.wait_for(lambda row: row.get("direction") == "received"
                            and row.get("message", {}).get("type") == "hello")
        response = await self.context.request.get(self.url + "/assets/build-manifest.json")
        if not response.ok:
            raise RuntimeError(f"served frontend manifest returned HTTP {response.status}")
        manifest = await response.body()
        (self.directory / "served-build-manifest.json").write_bytes(manifest)
        self.observations["served_manifest_sha256"] = hashlib.sha256(manifest).hexdigest()
        await self.capture("opened")

    async def submit(self, phase, content, subject):
        from playwright.async_api import expect

        origin = self.index
        self.checks[f"browser_{phase}_input_from_composer"] = False
        self.checks[f"browser_{phase}_durable_receipt"] = False
        await self.editor().fill(content)
        await expect(self.page.locator(".composer .send")).to_be_enabled()
        # The actual keyboard/composer handler creates the IDs and sends.
        await self.editor().press("Enter")
        sent = await self.wait_for(lambda row: row.get("direction") == "sent"
                                  and row.get("message", {}).get("type") == "submit", after=origin)
        submission = sent["message"]
        hello = next((row for row in reversed(self.controls)
            if row.get("direction") == "received" and row.get("socket_id") == sent["socket_id"]
            and row["index"] < sent["index"] and row.get("message", {}).get("type") == "hello"), None)
        capabilities = hello["message"].get("capabilities", {}) if hello else {}
        # The unified Send handler prefers steer only when this socket's hello
        # advertises it. Later capabilities frames negotiate surfaces; they do
        # not replace these input-mode capabilities.
        expected_mode = "steer" if capabilities.get("steer") is True else "append"
        self.observations[f"{phase}_input_mode"] = {
            "socket_id": sent["socket_id"], "hello_index": hello["index"] if hello else None,
            "capabilities": capabilities, "expected_mode": expected_mode,
            "submitted_mode": submission.get("mode"),
        }
        self.checks[f"browser_{phase}_input_mode_matches_capability"] = bool(
            hello and hello["message"].get("schema_version") == 2
            and capabilities.get("append") is True and capabilities.get("durable_input_receipts") is True
            and submission.get("mode") == expected_mode)
        if (submission.get("content") != content or submission.get("subject_id") != subject
                or submission.get("schema_version") != 2
                or not self.checks[f"browser_{phase}_input_mode_matches_capability"]
                or not all(isinstance(submission.get(key), str) and submission[key]
                           for key in ("submission_id", "message_id"))):
            raise AssertionError("browser submitted a different input or subject")
        self.checks[f"browser_{phase}_input_from_composer"] = True
        self.submission_keys.add("submission." + submission["submission_id"])
        receipt = await self.wait_for(lambda row: row.get("direction") == "received"
            and row.get("socket_id") == sent["socket_id"]
            and row.get("message", {}).get("type") == "receipt"
            and row["message"].get("submission_id") == submission["submission_id"]
            and row["message"].get("stage") == "accepted", after=sent["index"])
        accepted = receipt["message"]
        placement = accepted.get("placement", {})
        self.observations[f"{phase}_input_mode"]["accepted_receipt"] = accepted
        self.checks[f"browser_{phase}_durable_receipt"] = bool(
            accepted.get("schema_version") == 2 and accepted.get("durability") == "durable"
            and accepted.get("accepted_commit") and accepted.get("event_id")
            and accepted.get("mode") == submission["mode"] and accepted.get("replayed") is False
            and placement.get("requested") == submission["mode"] and placement.get("actual") == "next_boundary")
        if not self.checks[f"browser_{phase}_durable_receipt"]:
            raise AssertionError("browser input receipt differs from its actual mode or durable placement")
        await self.capture(f"{phase}-submitted")
        return submission

    async def capture(self, phase):
        # These reads inspect the live DOM; they never modify application
        # state, storage, WebSockets or a ledger-derived result.
        state = await self.page.evaluate("""() => {
            const scroll = document.querySelector('.im-scroll');
            const viewport = scroll?.getBoundingClientRect();
            const rows = scroll ? [...scroll.querySelectorAll('.im-msg[data-message-key]')].map(row => {
                const bounds = row.getBoundingClientRect();
                return {key: row.dataset.messageKey, entry_key: row.dataset.entryKey,
                    text: (row.querySelector('.im-line') ?? row).innerText,
                    offset: bounds.top - viewport.top,
                    visible: bounds.bottom > viewport.top && bounds.top < viewport.bottom};
            }) : [];
            return {url: location.href, source: document.querySelector('.shell')?.dataset.source,
                viewport: {width: innerWidth, height: innerHeight, dpr: devicePixelRatio},
                reduced_motion: matchMedia('(prefers-reduced-motion: reduce)').matches,
                rows, anchor: rows.find(row => row.visible) ?? null,
                scroll: scroll ? {top: scroll.scrollTop, height: scroll.scrollHeight, client: scroll.clientHeight} : null,
                announcements: [...document.querySelectorAll('[aria-live]')].map(el => ({
                    politeness: el.getAttribute('aria-live'), text: el.textContent}))};
        }""")
        state["editor"] = await self.editor().evaluate("""el => ({
            draft: el.value, focused: el.getRootNode().activeElement === el,
            selection_start: el.selectionStart, selection_end: el.selectionEnd
        })""")
        for row in state["rows"]:
            row["stable_fact"] = row["key"] in self.committed_keys | self.submission_keys
        state["anchor"] = next((row for row in state["rows"] if row["visible"] and row["stable_fact"]), None)
        save(self.directory / f"{phase}.dom.json", state)
        await self.page.screenshot(path=str(self.directory / f"{phase}.png"), caret="initial")
        (self.directory / f"{phase}.aria.yml").write_text(await self.page.locator("body").aria_snapshot())
        self.captures.append(phase)
        return state

    async def observe_output(self, phase, step_id, markers):
        self.checks[f"browser_{phase}_committed_output"] = False
        await self.page.locator('nav[aria-label="视图"]').get_by_text("Inspect", exact=True).click()
        output = self.page.locator(f'.timeline [data-message-id="{step_id}"] .entry-body')
        try:
            await output.first.wait_for(state="visible")
            texts = await output.all_inner_texts()
            self.checks[f"browser_{phase}_committed_output"] = any(
                all(marker in text for marker in markers) for text in texts)
            self.observations[f"{phase}_output"] = {"step_id": step_id, "rendered_parts": texts}
        except Exception as error:
            self.observations[f"{phase}_output"] = {"step_id": step_id, "error": str(error)}
        await self.capture(f"{phase}-output")
        await self.page.locator('nav[aria-label="视图"]').get_by_text("对话", exact=True).click()

    async def prepare_restart(self, draft):
        await self.editor().fill(draft)
        dimensions = await self.page.locator(".im-scroll").evaluate(
            "el => ({height: el.scrollHeight, client: el.clientHeight})")
        if dimensions["height"] - dimensions["client"] > 80:
            await self.page.locator(".im-scroll").hover()
            await self.page.mouse.wheel(0, -max(80, (dimensions["height"] - dimensions["client"]) / 2))
            await self.page.evaluate("() => new Promise(done => requestAnimationFrame(() => requestAnimationFrame(done)))")
        self.before_restart = await self.capture("before-restart")
        self.restart_index = self.index
        self.restart_socket_id = next(row["socket_id"] for row in reversed(self.controls)
            if row.get("direction") == "received" and row.get("message", {}).get("type") == "hello")
        self.restart_committed_keys = self.committed_keys.copy()
        self.checks["browser_disconnect_observed"] = False
        self.checks["browser_reconnected"] = False

    async def observe_stopped(self):
        stopped = self.record("owned_cli_stopped")
        closed = await self.wait_for(lambda row: row["kind"] == "socket_closed"
            and row["socket_id"] == self.restart_socket_id, after=self.restart_index)
        self.reconnect_after = max(stopped["index"], closed["index"])
        self.checks["browser_disconnect_observed"] = True
        await self.capture("runtime-stopped")

    async def observe_restarted(self):
        hello = await self.wait_for(lambda row: row.get("direction") == "received"
            and row.get("message", {}).get("type") == "hello"
            and row["socket_id"] != self.restart_socket_id, after=self.reconnect_after)
        self.checks["browser_reconnected"] = True
        if self.restart_committed_keys:
            self.checks["browser_original_committed_replay_observed"] = False
            await self.wait_for(lambda row: row.get("direction") == "received"
                and row["socket_id"] == hello["socket_id"]
                and row.get("message", {}).get("type") == "timeline_part"
                and "part." + json.dumps([row["message"].get("message_id"), row["message"].get("part_id")],
                    ensure_ascii=False, separators=(",", ":")) in self.restart_committed_keys,
                after=self.reconnect_after)
            self.checks["browser_original_committed_replay_observed"] = True
        await self.page.evaluate("() => new Promise(done => requestAnimationFrame(() => requestAnimationFrame(done)))")
        after = await self.capture("runtime-reconnected")
        before = self.before_restart
        self.checks["browser_draft_retained_after_restart"] = after["editor"]["draft"] == before["editor"]["draft"]
        if before["editor"]["focused"]:
            self.checks["browser_focus_retained_after_restart"] = after["editor"]["focused"]
        else:
            self.observations["focus"] = {"observed": False, "reason": "editor was not focused at restart boundary"}
        # A still-growing stream is allowed to change or become a committed
        # round. Only original inputs and already committed parts are stable.
        self.checks["browser_existing_messages_retained_once"] = self.stable_messages_match(after)
        anchor = before["anchor"]
        scroll = before["scroll"]
        if anchor and scroll and 0 < scroll["top"] < scroll["height"] - scroll["client"] - 32:
            match = next((row for row in after["rows"] if row["key"] == anchor["key"]), None)
            drift = abs(match["offset"] - anchor["offset"]) if match else None
            self.observations["reading_anchor"] = {"observed": True, "key": anchor["key"],
                "before_offset": anchor["offset"], "after_offset": match["offset"] if match else None,
                "absolute_drift_css_px": drift, "tolerance_css_px": 2}
            self.checks["browser_reading_anchor_retained"] = drift is not None and drift <= 2
        else:
            self.observations["reading_anchor"] = {"observed": False,
                "reason": "actual journey content did not establish a middle reading position"}
        queries = [row["message"] for row in self.controls if row["index"] > self.restart_index
                   and row.get("direction") == "sent" and row.get("message", {}).get("type") == "query_submission"]
        self.observations["reconnect"] = {"hello_socket_id": hello["socket_id"], "actual_receipt_queries": queries,
            "receipt_query_scope": "already committed submissions need not be queried again"}

    def stable_messages_match(self, state):
        old = {row["key"]: row["text"] for row in self.before_restart["rows"] if row["stable_fact"]}
        counts = Counter(row["key"] for row in state["rows"])
        return bool(old) and all(counts[key] == 1 and any(
            row["key"] == key and row["text"] == text for row in state["rows"]) for key, text in old.items())

    async def finish_journey(self):
        # Replay can arrive after hello and after the first paint. Recheck the
        # original facts after the continued model result, on the same page.
        state = await self.capture("journey-finished")
        self.checks["browser_existing_messages_retained_once_at_finish"] = self.stable_messages_match(state)

    async def close(self, *, capture_timeout=15, cleanup_timeout=10):
        errors = []
        interruptions = []
        pending_interrupt = None
        cleanup = {"capture_timeout_seconds": capture_timeout, "owner_timeout_seconds": cleanup_timeout}

        def failed(stage, error):
            nonlocal pending_interrupt
            errors.append(f"{stage}: {type(error).__name__}: {error}")
            if not isinstance(error, Exception):
                if pending_interrupt is None:
                    pending_interrupt = error
                interruptions.append({"stage": stage, "type": type(error).__name__, "message": str(error)})

        if self.page is not None and not self.page.is_closed():
            try:
                async with asyncio.timeout(capture_timeout):
                    await self.capture("final")
                cleanup["final_capture"] = "complete"
            except BaseException as error:
                cleanup["final_capture"] = "failed"
                failed("final browser capture", error)
        self.checks["browser_no_page_errors"] = not self.page_errors
        stream_parts = self.counts[("received", "stream_part")]
        self.observations["streaming"] = {"observed": stream_parts > 0, "received_stream_parts": stream_parts,
            "scope": "actual public wire frames only; no stream-quality or latency claim"}
        self.observations["accessibility"] = {"aria_snapshot_phases": self.captures, "screen_reader_tested": False}
        for name, owner, operation in [("context", self.context, "close"), ("browser", self.browser, "close"),
                                       ("driver", self.driver, "stop")]:
            if owner is not None:
                try:
                    async with asyncio.timeout(cleanup_timeout):
                        await getattr(owner, operation)()
                    cleanup[name] = "complete"
                except BaseException as error:
                    cleanup[name] = "failed"
                    failed(f"{name} cleanup", error)
            else:
                cleanup[name] = "not_created"
        try:
            self.log.close()
        except BaseException as error:
            failed("browser event log close", error)
        # A timed-out/cancelled close is unknown, even if a later owner's close
        # returns. Never report that the browser was cleaned up normally.
        self.checks["browser_cleanup_complete"] = not errors
        report = {"checks": self.checks, "observations": self.observations, "errors": errors,
                  "page_errors": self.page_errors, "cleanup": cleanup, "interruptions": interruptions,
                  "scope": "one live page and owned CLI/model journey; no physical device, screen-reader, long-run or full UI acceptance"}
        try:
            save(self.directory / "summary.json", report)
        except BaseException as error:
            failed("browser summary retention", error)
            self.checks["browser_cleanup_complete"] = False
        if pending_interrupt is not None:
            pending_interrupt.shenko_browser_cleanup = report
            raise pending_interrupt
        return report
