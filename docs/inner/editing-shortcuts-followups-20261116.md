# PR #2958：原生编辑快捷键收尾验证

日期：2026-11-16。原始 PR HEAD：`4c5fbaee88bda4f47b238697de03491747996b14`，父提交 `fed98c8e1d`。

对照浏览器为本机 Chromium `156.0.8078.79`。Moli 使用原始 HEAD 和修复工作树分别构建的本地 debug 二进制；关闭 Rust 调试信息，不改变优化级别或编辑功能。修复构建在拆分提交之前完成，验证的是下面五个修复提交合起来的源码，不是 GitHub CI release 产物。

## 修复及对照结果

以下差异均在原始 Moli 构建上复现，并与真实 Chromium 输入操作对照。不是只回放协议事件，也没有通过脚本直接派发键盘事件替代原生输入。

| 场景 | 原始 HEAD | 修复后 / Chromium |
| --- | --- | --- |
| 输入 `a`，`textContent=''`，再按 Shift+KeyA | 编辑器为空 | `A` |
| 编辑 `<b>` 后撤销，旁边有 iframe | iframe Window、Document 和窗口变量丢失 | 三者保留；redo 也不重载 iframe |
| cut 回调把选区从 AB 改成 CD | 删除 CD，剪贴板得到 AB | 删除 CD，剪贴板得到 CD |
| readonly 输入框 Ctrl+X | 覆盖原剪贴板 | 不改值、不覆盖剪贴板 |
| contenteditable 内嵌 input 执行剪切 | 事件发给外层，不剪切 input | input 收到事件并剪切其选区 |
| `keydown` 的 `x` 回调把焦点从 A 转到 B | 仍剪切 A | 剪切 B |
| A、B 依次输入，再聚焦 A 撤销 | 栈顶目标不同，撤销不推进 | 撤销 B，焦点回到 B |
| 上一场景先移除 B | 无法撤销 A | 跳过失效 B 记录，撤销 A |
| 编辑过的文本被脚本移出编辑器后撤销 | 改写外部文本 | 外部文本保持不变 |
| `<b>ab▏</b><i>cd</i>` 前向 Delete | `abcd` | `abd` |
| 空剪贴板 / 只有自定义 MIME，粘贴到选中的富文本 | 删除选区 | 保留选区；textarea 仍执行空替换 |
| 把 `a<br>b` 复制到 textarea | `ab` | `a\nb` |
| 把 `first\nsecond` 粘贴到普通富文本编辑器 | 显示成一行 | 保留两行 |

富文本多行粘贴的 DOM 结构允许不同：Moli 生成文本与 `<br>`，Chromium 在这个样例中生成 `<div>`。对照断言验证可见文本；另外检查撤销、节点身份、选区和纯文本转义。

## 修改边界

1. `9cc59a32e7`：`textContent` 批量替换绕过了普通 `removeChild` 的 live Range 更新。在旧祖先关系仍可读取时，按逆序执行已有的移除更新。未改变 Shadow Selection 投影规则，也未放宽只读 / inert 编辑检查。
2. `7e76eebf29`：撤销只移除不需要的子节点，并在游标位置插入缺少的节点；已在正确位置的兄弟节点保持连接。校验变动节点及待恢复子树的宿主归属，拒绝拉回脚本移到其他树中的节点。撤销栈按 Document 消费，跳过失效条目，并在 beforeinput、focus 回调后重新校验。
3. `486bb7e3d9`：快捷键重新读取 keydown 后的焦点，文本控件优先于外层编辑宿主。cut 返回后再读取选区和可编辑状态。没有可用纯文本时，区分富文本与文本控件的粘贴默认行为。
4. `66459a5f71`：跨相邻可编辑 Text 节点删除字符，复用 UTF-16 scalar 边界处理。保留原 caret，并扩大本次历史快照的捕获区域，使 undo 能恢复邻接文本。复用焦点初始化已有的编辑边界判断。
5. `a25091152d`：Selection 的可见文本包含选中的 `<br>`，标准 `Range.toString()` 仍只拼接字符数据。普通富文本粘贴换行通过节点插入完成，不把纯文本解析成 HTML。`plaintext-only` 和保留换行的 white-space 模式继续保留文本中的 LF。

这些改动没有增加布局刷新条件。检查新粘贴内容的 `innerText` 时，测试显式截图以发布几何快照；输入处理本身不强制刷新 FrozenLayoutTree。

## 自动化覆盖

Rust 回归放在 `moli-renderer-v8/src/script_vm/tests/dom_xhr/editing_shortcuts.rs` 及其 `history`、`clipboard`、`text_boundaries` 子模块，覆盖：

- textContent、innerHTML、replaceChildren、remove 的折叠 / 非折叠选区及后续实际输入；
- iframe 与 Custom Element 兄弟节点的连接状态、undo / redo、跨输入框历史；
- beforeinput / focus 回调移除或 adopt 目标，脚本将文本或待恢复子树移入另一棵树；
- cut 回调改变选区或 readonly，嵌套控件事件目标，空 / 自定义 MIME 剪贴板；
- 相邻文本的 Delete / Backspace、UTF-16 emoji、只读子树边界、跨节点删除的 undo；
- `<br>` 与标准 Range 文本的区别，换行粘贴、纯文本转义、文本节点 split 后的 undo，以及 plaintext-only / pre / pre-wrap。

新增 `moli-cdp-smoke/moli_cdp_smoke/groups/editing_shortcuts.py`，接入现有 `dom-input` 组。每个场景使用新 BrowserContext 和 Document，避免剪贴板、焦点或历史相互污染。本次单独执行该 workflow，Moli 与 Chromium 各 **16/16** 通过。既有 `run_keyboard_modifier_workflow()` 也在两端通过。

另外，真实 Playwright 操作验证了四种移除方式 × 两种选区状态的八个对照，修复后均迁回编辑器并能输入 `A`。

## Rust 验证

在全部修复后的工作树、拆分提交之前运行，未开启失败重试：

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --no-fail-fast \
  -E 'test(editing_shortcuts) | test(keyboard_modifiers) | test(selection_worlds)' \
  --stress-count 5 --flaky-result fail
cargo nextest run --no-fail-fast
```

结果：fmt、Clippy 通过；定向 **24 个测试 × 5 轮，全部通过**；全量 **23,346 passed、20 skipped、0 failed**。全量 Nextest run ID：`f6546478-47a7-43d0-be21-d6e45f5b944f`。原始 HEAD 的基线为 **23,332 passed、20 skipped、0 failed**。

使用 `CARGO_BUILD_JOBS=24`、`NEXTEST_TEST_THREADS=24`、`NEXTEST_RETRIES=0`。没有修改测试超时、增加重试或跳过失败用例。本次没有独立重跑整个 CDP smoke 组，不能将上述专项结果称为完整 CDP CI 通过。

## 扩展探针确认的既有差异

以下两项在原始 HEAD 已存在，不属于上述报告场景的回归修复，也没有作为 Chromium 一致性通过项计数：

- 前向 Delete 遇到 `contenteditable=false` 原子元素时，Chromium 可以删除整个元素而不改写其内部文本；Moli 仍停在该边界。跨普通 Text 节点删除已经修复，原子节点删除需要另行处理。
- `contenteditable=plaintext-only` 在默认样式下保存了 LF，但可见文本仍将其折叠；Chromium 保留显示换行。本次保留其原有文本节点表示，避免因富文本换行处理而改变 `textContent`。普通 contenteditable 的换行粘贴已修复。
