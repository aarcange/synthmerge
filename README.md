# synthmerge

**AI-powered conflict resolution for Git**

`synthmerge` is a minimalistic command-line tool that leverages AI to automatically resolve conflicts arising from Git commands. Built on the research of the [Patchpal project](https://gitlab.com/patchpal-ai), it provides a pure AI inference layer that seamlessly integrates with your existing Git workflow. While the AI generates code solutions, all code reviews and approvals remain within your favorite code editor.

Instead of relying on a single model, `synthmerge` runs a **parallel inference engine** to seek the **AI collective consensus**.

---

## 🌟 Core Principles

1. **Specialized AI Layer**  
   Dedicated AI inference system that complements Git without duplicating its core functionality

2. **Git Integration**  
   Leverages Git's `diff3` conflict [markers](git-conflict-solutions-marker.md) as the foundation (requires `git config merge.conflictStyle diff3`)

3. **Editor Agnostic**  
   Compatible with any development environment (VS Code, Emacs, Vim, etc.)

---

## ✨ Key Features

- **Universal Git Operation Support**  
  Seamlessly integrates with all Git operations that create conflicts:
  - `cherry-pick`
  - `merge`
  - `rebase`
  - `revert`
  - `stash pop`

- **Model Flexibility**  
  No fine-tuning required, any instruct large language model can be used

- **Parallel Multi-AI Endpoint Support**  
  Simultaneously queries multiple AI models to resolve conflicts:
  - [Patchpal-backend](https://gitlab.com/patchpal-ai/patchpal-backend) (fine-tuned specifically for conflict resolution)
  - Self-hosted open-weight open source LLMs with OpenAI-compatible endpoints (llama.cpp/vLLM)
  - OpenAI (via OpenAI API)
  - Gemini (via OpenAI-compatible API)
  - Claude (via Anthropic API)

- **Parameter Variants Support**  
  Each AI endpoint can be configured with multiple parameter variants to run multiple inference strategies:
  - Different reasoning effort levels (high, medium, low)
  - Temperature, top_p, top_k, min_p sampling parameters
  - Context handling options (context: no_diff: no_training: layout: flags)
  - Custom JSON parameters that can be injected into the request payload from the YAML configuration (either at the endpoint level or in each variant)
  - Number of beams for Patchpal AI endpoint (n_beams)

- **Endpoint Fallbacks**  
  Configure fallback endpoints to ensure high availability and redundancy:
  - If all variants of an endpoint fail, synthmerge automatically queries the configured fallbacks in order
  - Fallback endpoints are fully independent configurations, allowing different model types, authentication methods, or parameters

- **Results Deduplication & Ranking**  
  Consolidates identical solutions and displays model and/or parameter variant agreement. If multiple models agree on a fix, that solution is ranked first.

- **Review Using Your Workflow**  
  - Resolved conflicts appear in your editor with model attribution
  - AI-generated code requires manual review before commit

- **Fail-Safe Design**  
  - When one model fails to resolve a conflict, Git's original conflict remains alongside solutions from other models for that hunk
  - Each AI endpoint can be configured with timeout, delay, and max_delay parameters
  - Custom root certificates can be added to the endpoint configuration
  - Wait time between requests can be specified per endpoint

- **Authentication Flexibility**  
  Supports multiple authentication methods for AI endpoints:
  - Static API keys via `api_key_file` (Bearer token) or `x_api_key_file` (X-API-Key header)
  - Google Cloud Platform service account authentication via `gcp_service_account_file` (automatically generates and caches OAuth2 access tokens)
  - Custom HTTP headers via `headers` configuration for additional authentication or routing requirements

- **Response Caching**  
  Built-in LMDB cache for API responses to avoid redundant requests:
  - Disable caching with `--no-cache` flag
  - Change the cache location with `--cache <path>` parameter
  - Overwrite mode to force new requests with `--cache-overwrite`
  - Import cache entries from another cache database with `--import-cache <path>`

- **Benchmark**  
  Built-in benchmarking tool (`synthmerge_bench`) for evaluating model accuracy on conflict resolution tasks

- **Context Lines Configuration**  
  Configurable context lines for code, diff, and patch to control the amount of surrounding information provided to AI models

- **Vibe Coding Mode**  
  Automatically resolve all conflicts and update the git index with `--vibe` flag. **Warning**: Vibe Coding is generally unsafe and should only be used for batch automation and verification purposes.

- **Vibe Continue Operation**  
  Use `--continue` with `--vibe` to automatically commit and continue cherry-pick, rebase, revert, or merge operations after resolving conflicts.

- **Marker Mode**  
  In vibe mode, synthmerge can detect cherry-picks requiring AI resolution, edit code beyond the original conflict markers and relocate conflicts to new positions in the file. To opt-out and strictly resolve conflicts within the diff3 conflict markers (matching non-vibe behavior), use the `--with-markers` option.

- **Conflict Relocation**  
  In vibe mode without `--with-markers`, synthmerge can relocate conflicts to their correct positions in the file when the original markers are misplaced.

---

## 🛠 How It Works

1. **Git sets up conflicts**  
   ```bash
   git config merge.conflictStyle diff3  # Must be set
   git cherry-pick -x <commit>           # Git detects conflicts
   ```

2. **synthmerge analyzes conflicts**  
   - Reads Git's `diff3` conflict markers
   - Extracts context (3 lines before/after conflict)
   - Generates precise AI prompt

3. **AI resolves conflict**  
   - Sends code + patch to configured endpoint
   - Receives resolved code

4. **Git gets updated**  
   - synthmerge inserts the AI resolution into existing diff3 conflicts with a new [solutions marker](git-conflict-solutions-marker.md)
   - You review in your editor

> ✅ Works also for git rebase, revert and merge conflict resolutions.

---

## 🚀 Usage

```bash
# Ensure Git is configured for diff3 conflict style
git config merge.conflictStyle diff3

# Attempt cherry-pick (will leave conflicts unresolved)
git cherry-pick -x <commit>

# Resolve conflicts with AI
synthmerge

# Review synthmerge resolved conflicts in each unmerged file ...
git diff --name-only --diff-filter=U

# ... or linearized in a single buffer to edit with ripgrep-edit
rg-edit -E vim -C10 -U -e '(?s)^<<<<<<<+ .*?^\|\|\|\|\|\|\|+ .*?^>>>>>>>+ '
rg-edit -E emacsclient -C10 -U -e '(?s)^<<<<<<<+ .*?^\|\|\|\|\|\|\|+ .*?^>>>>>>>+ '

# For automatic resolution and git index update, use the `--vibe` flag
synthmerge --vibe

# For automatic resolution, git index update and continuing the operation
git checkout -f v6.12 && git cherry-pick -x -m 1 v6.18-rc5..v6.18-rc6~2^2 || synthmerge --vibe --continue
git rebase -i v6.18-rc5 v6.18-rc6~2^2 --onto v6.12 || synthmerge --vibe --continue

# Disable caching for a run
synthmerge --no-cache

# Import cache from src to dst
synthmerge --import-cache /path/to/src --cache /path/to/dst

# Overwrite cache entries (force new API requests)
synthmerge --cache-overwrite
```

---

## ⚙️ Configuration

Create `~/.config/synthmerge.yaml` based on [synthmerge.yaml](./synthmerge.yaml)

---

## 🌐 Supported AI Endpoints

| Endpoint Type | Example Configuration | Notes |
|---------------|------------------------|-------|
| **Patchpal-backend** | `type: "patchpal"` | Fine-tuned for patch resolution |
| **OpenAI protocol** | `type: "openai"` | Self-hosted LLMs (e.g., `llama.cpp`) and Gemini |
| **Anthropic protocol** | `type: "anthropic"` | Claude models |

> ✅ **Gemini supports a compatible OpenAI endpoint**  
> ✅ **Models work with stock weights** – the prompt engineering simulates Patchpal's fine-tuned behavior.

---

## ⚙️ Context Layout Configuration

The `context: layout:` configuration provides fine-grained control over how information is structured in LLM requests.

- **Prompt placement**: The highest-performing models (including Gemini 3 Flash, regardless of reasoning effort settings) achieve optimal results when critical directives are positioned closest to the generation context
- **Gemini 2.5 thinking models exception**: Models with `reasoning_effort != none` (all Gemini 2.5 Pro variants) require the prompt explaining the challenge to be placed at the beginning of the system message
- **Layout flexibility**: This configuration enables each model to select its optimal information structure, maximizing performance through tailored context organization

### Available layout elements:
- `prompt`: The high-level prompt explaining the challenge
- `training`: The synthetic training examples
- `diff`: The full git diff showing all other changes of the commit

### Context control flags:
- `no_diff`: Disable diff inclusion in context
- `no_training`: Disable training examples in context

### Configuration examples:
```yaml
# Set layout at endpoint level
context:
  layout:
    system_message:
      - prompt
    user_message:
      - training
      - diff

# Override layout in a variant
variants:
  - name: "no_diff"
    context:
      no_diff: true
  - name: "no_training"
    context:
      no_training: true
```

The layout can be configured either at the endpoint level or in individual variants, but not both simultaneously in the same endpoint.

---

## 🛠️ GBNF EBNF Grammar Support

To enable structured output constraints for OpenAI-compatible endpoints, add the `grammar` parameter.

- **llama.cpp**: GBNF grammar support
- **vLLM**: EBNF grammar support

```yaml
# For llama.cpp (GBNF)
endpoints:
  - name: "llama.cpp vulkan"
    url: "http://localhost:8811/v1/chat/completions"
    type: "openai"
    grammar: "gbnf"
    # ... other configuration parameters

# For vLLM (EBNF)
endpoints:
  - name: "vllm endpoint"
    url: "https://localhost:443/v1/chat/completions"
    type: "openai"
    grammar: "ebnf"
    # ... other configuration parameters
```

### When to Use Grammar

Some models exhibit high error rates or do not benefit from reasoning before generating code. In these cases, grammar constraints force the model to produce code directly, bypassing intermediate reasoning steps to improve reliability and accuracy.

## 🎯 Primary Endpoints

The `primary: true` flag designates endpoints as "primary" participants in the AI consensus.

- **Hard Conflicts**: Only primary endpoints resolve "clean" hunks
- **No Retries**: Only primary endpoints are required to participate in resolving all conflicts

```yaml
endpoints:
  - name: "Primary Model"
    primary: true
```

## 🔁 Endpoint Fallbacks

The `fallbacks` list allows you to specify backup endpoints that are automatically used if the primary endpoint fails. This is useful for:

- **High Availability**: Ensure conflict resolution continues even if one AI provider is down
- **Safety Filters**: Bypass provider-specific safety filters, such as content recitation blocks or other cyber safeguards, by switching to an endpoint with different moderation policies

### Fallback Behavior

1. **Trigger**: Fallbacks are triggered only when **all variants** of the endpoint fail
2. **Order**: Fallbacks are tried in the order they are listed in the configuration

### Configuration Example

```yaml
endpoints:
  - name: "Gemini 3.8 Flash"
    url: "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
    type: "openai"
    primary: true
    api_key_file: "~/.keys/gemini.api_key"
    json:
      model: "gemini-3.8-flash"
    fallbacks:
      - name: "fallback Gemini 3.7 Flash"
        url: "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
        type: "openai"
        primary: true
        api_key_file: "~/.keys/gemini.api_key"
        json:
          model: "gemini-3.7-flash"
      - name: "fallback OpenAI gpt-6-aura"
        url: "https://api.openai.com/v1/chat/completions"
        type: "openai"
        primary: true
        api_key_file: "~/.keys/openai.api_key"
        json:
          model: "gpt-6-aura"
```

In this example, if all variants of "Gemini 3.8 Flash" fail (e.g., due to a content filter or service outage), synthmerge will try "fallback Gemini 3.7 Flash" first. If that also fails, it will try "fallback OpenAI gpt-6-aura".

## 🔤 Markdown Backtick Support

Markdown backtick fences are enabled by default. However, if a specific model gets confused by the superflous fences they can be disabled:

```yaml
endpoints:
  - name: "Model without backticks"
    use_backticks: false
```

## 🎨 Emacs Integration

`synthmerge` ships with a modified `smerge-mode` plugin for that provides a visual interface to review and select AI-generated solutions alongside the original conflict [markers](git-conflict-solutions-marker.md).

### smerge-refine Enhancement

The enhanced `smerge-refine` feature shows fine-grained differences between:

- **Base → Remote**: Highlights the word diff of the "patch" from section 2 to 3
- **Local → AI**: Highlights the word diff between the "code" "ai patched code" from section 1 to 4

This simutaneous dual word diff visualization helps you quickly assess:
- What are the remote changes that needs to be applied to the local code
- What changes have been applied by the AI to the local code

### Interactive Selection

You can interact with the refined view in several ways:

1. **Right-click on AI solution**: Opens a context menu to keep the AI version
2. **`M-x smerge-keep-current`**: Accept the version currently under point
3. **`M-x smerge-keep-ai`**: Explicitly select the AI-generated solution
4. **Navigation**: Use the standard smerge-mode `smerge-vc-next-conflict` `smerge-next` `smerge-prev`

This makes it easy to:
- Verify the AI properly resolved the conflict
- Check that your local changes are preserved
- Understand exactly what the AI modified
- Quickly accept or reject the AI solution

```elisp
(add-to-list 'load-path "~/.../synthmerge/emacs/")
```

## 🛠 Installation

### Fedora

A Fedora Copr package is available:

1. **Install Synthmerge**:

   ```bash
   sudo dnf copr enable vittyvk/synthmerge
   sudo dnf install synthmerge
   ```

2. **Configuration**:
   ```bash
   cp -a /usr/share/synthmerge/synthmerge.yaml ~/.config/
   $EDITOR ~/.config/synthmerge.yaml
   ```

### From source code

1. **Install Synthmerge**:
   ```bash
   git clone https://gitlab.com/aarcange/synthmerge.git
   cd synthmerge
   cargo build --release
   sudo cp target/release/synthmerge /usr/local/bin/
   ```

2. **Configuration**:
   ```bash
   cp synthmerge.yaml ~/.config/
   $EDITOR ~/.config/synthmerge.yaml
   ```

---

## 📊 Benchmark Statistics

The following statistics were generated using the `synthmerge_bench` tool on a C language dataset to evaluate model performance on conflict resolution tasks. These results may vary depending on prompt, context, and other variables. 

**Accuracy** checks if the AI resolved conflict is an exact match including all spaces, tabs, and newlines.

**Accuracy (aligned)** checks equality of whitespace patterns up until the first non-whitespace character, ignoring differences in lines without non-whitespace characters and whitespace variations after the first non-whitespace character (i.e. Python equivalence).

**Accuracy (stripped)** deletes all whitespaces and newlines (i.e. C/C++/Rust/JavaScript equivalence).

This measurement used only new test data never exposed to the model during the fine tuning process.

![Benchmark Results](https://gitlab.com/aarcange/synthmerge-assets/-/raw/main/synthmerge_bench-20261007.jpg)

### The Numbers

```
Model: Gemini 3.8 Flash (medium) {0.3.16}
  Accuracy: 72.19% (815/1129)
  Accuracy (aligned): 76.26% (861/1129)
  Accuracy (stripped): 79.81% (901/1129)
  Error Rate: 0.00% (0/1129)
  Average input tokens: 5412.12
  Average output tokens: 2872.10
  Average duration: 8.93 s

Model: AI Consensus: Gemini 3.1 Pro + Claude Opus 4.6 + Patchpal
  Accuracy: 71.74% (810/1129)
  Accuracy (aligned): 74.84% (845/1129)
  Accuracy (stripped): 77.68% (877/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 5883.78

Model: AI Consensus: Claude Opus 4.6 + Gemini 3.1 Pro + Patchpal
  Accuracy: 71.48% (807/1129)
  Accuracy (aligned): 74.40% (840/1129)
  Accuracy (stripped): 77.59% (876/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 5838.40

Model: Claude Opus 5 (adaptive) {0.3.16} # thinking adaptive
  Accuracy: 71.39% (806/1129)
  Accuracy (aligned): 75.02% (847/1129)
  Accuracy (stripped): 79.10% (893/1129)
  Error Rate: 0.00% (0/1129)
  Average input tokens: 7168.88
  Average output tokens: 444.21
  Average duration: 6.01 s

Model: Gemini 3.7 Flash (medium) # reasoning_effort: medium
  Accuracy: 71.21% (804/1129)
  Accuracy (aligned): 75.20% (849/1129)
  Accuracy (stripped): 78.57% (887/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 6410.88
  Average duration: 4.14 s

Model: Claude Opus 5.5 (adaptive) {0.3.16} # thinking adaptive
  Accuracy: 70.77% (799/1129)
  Accuracy (aligned): 74.93% (846/1129)
  Accuracy (stripped): 78.83% (890/1129)
  Error Rate: 0.00% (0/1129)
  Average input tokens: 7170.88
  Average output tokens: 397.98
  Average duration: 4.93 s

Model: AI Consensus: Gemini 3.1 Pro (low) + Claude Opus 4.6 (adaptive) + Gemini 3.5 Flash (none)
  Accuracy: 70.77% (799/1129)
  Accuracy (aligned): 74.40% (840/1129)
  Accuracy (stripped): 77.77% (878/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 5847.21

Model: AI Consensus: Gemini 3.1 Pro + Claude Opus 4.6
  Accuracy: 70.68% (798/1129)
  Accuracy (aligned): 74.22% (838/1129)
  Accuracy (stripped): 77.24% (872/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 5907.26

Model: gpt-5.5 (high)
  Accuracy: 70.50% (796/1129)
  Accuracy (aligned): 74.22% (838/1129)
  Accuracy (stripped): 78.12% (882/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 4817.62
  Average duration: 9.60 s

Model: gpt-5.6-sol (xhigh) {0.3.16}
  Accuracy: 70.06% (791/1129)
  Accuracy (aligned): 73.78% (833/1129)
  Accuracy (stripped): 77.59% (876/1129)
  Error Rate: 0.00% (0/1129)
  Average input tokens: 4224.94
  Average output tokens: 349.05
  Average duration: 5.57 s

Model: gpt-5.6-sol (high)
  Accuracy: 69.71% (787/1129)
  Accuracy (aligned): 73.43% (829/1129)
  Accuracy (stripped): 77.15% (871/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 4309.43
  Average duration: 3.91 s

Model: Gemini 3.6 Flash (medium) # reasoning_effort: medium
  Accuracy: 69.35% (783/1129)
  Accuracy (aligned): 73.96% (835/1129)
  Accuracy (stripped): 77.33% (873/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 7537.06
  Average duration: 9.70 s

Model: gpt-5.5 (medium)
  Accuracy: 69.35% (783/1129)
  Accuracy (aligned): 73.07% (825/1129)
  Accuracy (stripped): 76.88% (868/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 4469.82
  Average duration: 5.64 s

Model: Gemini 3.1 Pro (high default) # reasoning_effort: high
  Accuracy: 69.35% (783/1129)
  Accuracy (aligned): 73.25% (827/1129)
  Accuracy (stripped): 76.53% (864/1129)
  Error Rate: 1.42% (16/1129)
  Average tokens: 7865.37
  Average duration: 14.26 s

Model: Claude Opus 4.6 (adaptive) # thinking adaptive
  Accuracy: 69.35% (783/1129)
  Accuracy (aligned): 72.63% (820/1129)
  Accuracy (stripped): 76.00% (858/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 6051.32
  Average duration: 7.70 s

Model: Claude Opus 4.6 (default)
  Accuracy: 69.09% (780/1129)
  Accuracy (aligned): 72.72% (821/1129)
  Accuracy (stripped): 76.17% (860/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 5769.34
  Average duration: 3.39 s

Model: gpt-5.6-sol (none)
  Accuracy: 68.64% (775/1129)
  Accuracy (aligned): 72.54% (819/1129)
  Accuracy (stripped): 76.35% (862/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 4192.87
  Average duration: 2.12 s

Model: Gemini 3.1 Pro (medium) # reasoning_effort: medium
  Accuracy: 68.47% (773/1129)
  Accuracy (aligned): 72.10% (814/1129)
  Accuracy (stripped): 75.47% (852/1129)
  Error Rate: 3.28% (37/1129)
  Average tokens: 6348.46
  Average duration: 9.95 s

Model: gpt-6.1-sol (xhigh) {0.3.16}
  Accuracy: 68.29% (771/1129)
  Accuracy (aligned): 72.10% (814/1129)
  Accuracy (stripped): 75.73% (855/1129)
  Error Rate: 0.00% (0/1129)
  Average input tokens: 4221.79
  Average output tokens: 388.53

Model: gpt-5.6-sol (medium)
  Accuracy: 68.20% (770/1129)
  Accuracy (aligned): 71.83% (811/1129)
  Accuracy (stripped): 75.64% (854/1129)
  Error Rate: 0.18% (2/1129)
  Average tokens: 4263.35
  Average duration: 3.53 s

Model: gpt-6.1-sol (high) {0.3.16}
  Accuracy: 68.02% (768/1129)
  Accuracy (aligned): 71.74% (810/1129)
  Accuracy (stripped): 75.29% (850/1129)
  Error Rate: 0.00% (0/1129)
  Average input tokens: 4221.79
  Average output tokens: 244.17
  Average duration: 6.89 s

Model: gpt-5.6-sol (low)
  Accuracy: 67.94% (767/1129)
  Accuracy (aligned): 71.57% (808/1129)
  Accuracy (stripped): 75.82% (856/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 4225.46
  Average duration: 2.69 s

Model: gpt-6-astra (xhigh) {0.3.16}
  Accuracy: 67.94% (767/1129)
  Accuracy (aligned): 71.66% (809/1129)
  Accuracy (stripped): 75.20% (849/1129)
  Error Rate: 0.00% (0/1129)
  Average input tokens: 4224.94
  Average output tokens: 405.50
  Average duration: 10.31 s

Model: Gemini 3.5 Flash (high default) # reasoning_effort: high
  Accuracy: 67.76% (765/1129)
  Accuracy (aligned): 74.05% (836/1129)
  Accuracy (stripped): 77.33% (873/1129)
  Error Rate: 0.18% (2/1129)
  Average tokens: 8575.48
  Average duration: 15.77 s

Model: Gemini 3.1 Pro (low default) # reasoning_effort: low
  Accuracy: 67.67% (764/1129)
  Accuracy (aligned): 71.39% (806/1129)
  Accuracy (stripped): 74.84% (845/1129)
  Error Rate: 3.10% (35/1129)
  Average tokens: 5759.32
  Average duration: 6.33 s

Model: Gemini 3.5 Flash (medium) # reasoning_effort: medium
  Accuracy: 67.05% (757/1129)
  Accuracy (aligned): 73.60% (831/1129)
  Accuracy (stripped): 76.97% (869/1129)
  Error Rate: 0.27% (3/1129)
  Average tokens: 7637.98
  Average duration: 11.68 s

# only the Patchpal Beam 0 is comparable to the non Patchpal models
Model: Patchpal AI 7B #0
  Accuracy: 67.05% (757/1129)
  Accuracy (aligned): 70.95% (801/1129) # might be duplicate with other beams
  Accuracy (stripped): 73.60% (831/1129) # might be duplicate with other beams
  Error Rate: 0.00% (0/1129)
  Average duration: 10.90 s
  Average prob: 92.9% (+- 7.2)
  Average prob (incorrect): 88.3% (+- 8.9)
  Average prob (stripped): 94.6% (+- 5.6)
  Average prob (aligned): 94.8% (+- 5.6)
  Average prob (correct): 95.0% (+- 5.3)

Model: Claude Opus 4.7 (adaptive) # thinking adaptive
  Accuracy: 67.05% (757/1129)
  Accuracy (aligned): 70.33% (794/1129)
  Accuracy (stripped): 73.60% (831/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 7508.22
  Average duration: 5.87 s

Model: Gemini 3.5 Flash (none default) # reasoning_effort: none
  Accuracy: 66.96% (756/1129)
  Accuracy (aligned): 72.19% (815/1129)
  Accuracy (stripped): 75.29% (850/1129)
  Error Rate: 0.27% (3/1129)
  Average tokens: 5351.88
  Average duration: 1.93 s

Model: Claude Opus 4.8 (adaptive) # thinking adaptive
  Accuracy: 66.78% (754/1129)
  Accuracy (aligned): 70.59% (797/1129)
  Accuracy (stripped): 73.87% (834/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 7503.43
  Average duration: 7.10 s

Model: Claude Sonnet 4.0 (default)
  Accuracy: 66.70% (753/1129)
  Accuracy (aligned): 70.42% (795/1129)
  Accuracy (stripped): 73.34% (828/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 5730.47
  Average duration: 7.03 s

Model: Gemini 3.5 Flash (low default) # reasoning_effort: low
  Accuracy: 66.61% (752/1129)
  Accuracy (aligned): 73.43% (829/1129)
  Accuracy (stripped): 76.62% (865/1129)
  Error Rate: 0.35% (4/1129)
  Average tokens: 6316.99
  Average duration: 6.02 s

Model: Gemini 3.6 Flash (default minimal) # reasoning_effort: minimal
  Accuracy: 65.99% (745/1129)
  Accuracy (aligned): 69.26% (782/1129)
  Accuracy (stripped): 72.10% (814/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 5397.78
  Average duration: 1.66 s

Model: Claude Opus 4.7 (default)
  Accuracy: 65.90% (744/1129)
  Accuracy (aligned): 68.91% (778/1129)
  Accuracy (stripped): 72.28% (816/1129)
  Error Rate: 0.35% (4/1129)
  Average tokens: 7144.33
  Average duration: 3.45 s

Model: Claude Opus 4.6 (no_diff)
  Accuracy: 65.28% (737/1129)
  Accuracy (aligned): 68.56% (774/1129)
  Accuracy (stripped): 71.74% (810/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 1297.93
  Average duration: 4.60 s

Model: Claude Sonnet 4.0 (no_diff)
  Accuracy: 65.19% (736/1129)
  Accuracy (aligned): 68.29% (771/1129)
  Accuracy (stripped): 71.48% (807/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 1184.14
  Average duration: 6.34 s

Model: Claude Sonnet 4.5 (default)
  Accuracy: 65.10% (735/1129)
  Accuracy (aligned): 70.06% (791/1129)
  Accuracy (stripped): 73.16% (826/1129)
  Error Rate: 0.27% (3/1129)
  Average tokens: 5735.29
  Average duration: 3.04 s

# temperature: 0.15 top_k: 20 top_p: 0.8 min_p: 0.00
# llama.cpp vulkan enable_thinking: false
Model: Qwen3.8-27B-UD-Q6_K_XL (gbnf)
  Accuracy: 65.10% (735/1129)
  Accuracy (aligned): 68.56% (774/1129)
  Accuracy (stripped): 72.63% (820/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 4577.64
  Average duration: 7.64 s

Model: Claude Sonnet 5 (adaptive) # thinking adaptive
  Accuracy: 65.10% (735/1129)
  Accuracy (aligned): 68.82% (777/1129)
  Accuracy (stripped): 71.83% (811/1129)
  Error Rate: 4.34% (49/1129)
  Average tokens: 8071.57
  Average duration: 7.09 s

Model: gpt-5.6-terra (medium)
  Accuracy: 64.66% (730/1129)
  Accuracy (aligned): 68.20% (770/1129)
  Accuracy (stripped): 71.83% (811/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 4248.54
  Average duration: 2.34 s

# temperature: 0.15 top_k: 20 top_p: 0.8 min_p: 0.00
# llama.cpp vulkan enable_thinking: false
Model: Qwen3.5-27B-UD-Q6_K_XL (gbnf)
  Accuracy: 63.86% (721/1129)
  Accuracy (aligned): 68.20% (770/1129)
  Accuracy (stripped): 71.48% (807/1129)
  Error Rate: 0.09% (1/1129)
  Average tokens: 4515.35
  Average duration: 35.35 s
  Average prob: 6.3% (+- 34.7)
  Average prob (incorrect): 1.1% (+- 27.8)
  Average prob (stripped): 12.7% (+- 34.4)
  Average prob (aligned): 13.8% (+- 34.3)
  Average prob (correct): 17.1% (+- 33.9)

Model: Gemini 3 Flash (none default) # reasoning_effort: none
  Accuracy: 64.13% (724/1129)
  Accuracy (aligned): 70.59% (797/1129)
  Accuracy (stripped): 73.43% (829/1129)
  Error Rate: 0.62% (7/1129)
  Average tokens: 5359.16
  Average duration: 1.73 s

# temperature 0.15 top_p: 0.8 top_k: 20 min_p: 0.00 reasoning_effort: high
Model: GLM 5.3 (high) {0.3.16}
  Accuracy: 63.15% (713/1129)
  Accuracy (aligned): 69.18% (781/1129)
  Accuracy (stripped): 72.72% (821/1129)
  Error Rate: 0.18% (2/1129)
  Average input tokens: 4192.11
  Average output tokens: 456.23
  Average duration: 8.56 s

Model: Gemini 3 Flash (none no_diff) # reasoning_effort: none
  Accuracy: 62.98% (711/1129)
  Accuracy (aligned): 68.02% (768/1129)
  Accuracy (stripped): 71.04% (802/1129)
  Error Rate: 2.04% (23/1129)
  Average tokens: 1084.24
  Average duration: 1.53 s

# temperature: 0.15 top_k: 20 top_p: 0.8 min_p: 0.00
# llama.cpp vulkan enable_thinking: false
Model: Qwen3.6-35B-A3B-UD-Q6_K_XL (gbnf)
  Accuracy: 62.09% (701/1129)
  Accuracy (aligned): 66.16% (747/1129)
  Accuracy (stripped): 69.26% (782/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 4555.88
  Average duration: 7.11 s
  Average prob: 8.9% (+- 36.7)
  Average prob (incorrect): 1.7% (+- 29.8)
  Average prob (stripped): 18.5% (+- 35.6)
  Average prob (aligned): 20.0% (+- 35.3)
  Average prob (correct): 23.6% (+- 34.7)

# temperature: 0.15 top_k: 20 top_p: 0.8 min_p: 0.00
# llama.cpp vulkan enable_thinking: false
Model: Qwen3.6-27B-UD-Q6_K_XL (gbnf)
  Accuracy: 62.00% (700/1129)
  Accuracy (aligned): 65.81% (743/1129)
  Accuracy (stripped): 69.35% (783/1129)
  Error Rate: 0.35% (4/1129)
  Average tokens: 4562.85
  Average prob: 5.9% (+- 36.5)
  Average prob (incorrect): 1.0% (+- 28.9)
  Average prob (stripped): 12.6% (+- 36.4)
  Average prob (aligned): 14.2% (+- 36.3)
  Average prob (correct): 17.0% (+- 35.9)

Model: Claude Sonnet 4.6 (default)
  Accuracy: 60.67% (685/1129)
  Accuracy (aligned): 63.86% (721/1129)
  Accuracy (stripped): 66.61% (752/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 5768.64
  Average duration: 3.57 s

Model: gpt-5.6-luna (medium)
  Accuracy: 58.90% (665/1129)
  Accuracy (aligned): 62.80% (709/1129)
  Accuracy (stripped): 66.25% (748/1129)
  Error Rate: 0.53% (6/1129)
  Average tokens: 4355.95
  Average duration: 3.34 s

Model: Claude Opus 4.8 (default)
  Accuracy: 58.90% (665/1129)
  Accuracy (aligned): 62.00% (700/1129)
  Accuracy (stripped): 64.48% (728/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 7132.77
  Average duration: 3.47 s

Model: Gemini 3.1 Flash Lite (none default) # reasoning_effort: none
  Accuracy: 57.93% (654/1129)
  Accuracy (aligned): 65.90% (744/1129)
  Accuracy (stripped): 69.71% (787/1129)
  Error Rate: 3.01% (34/1129)
  Average tokens: 5398.07
  Average duration: 1.20 s

# temperature: 0.15 top_k: default (40) top_p: default (0.95) min_p: 0.01
# llama.cpp vulkan
Model: Devstral-Small-2-24B-Instruct-2512-UD-Q6_K_XL (default)
  Accuracy: 57.22% (646/1129)
  Accuracy (aligned): 64.30% (726/1129)
  Accuracy (stripped): 67.32% (760/1129)
  Error Rate: 0.27% (3/1129)
  Average tokens: 4583.82
  Average duration: 14.04 s
  Average prob: 2.1% (+- 31.0)
  Average prob (incorrect): 0.4% (+- 21.8)
  Average prob (stripped): 4.8% (+- 33.0)
  Average prob (aligned): 5.1% (+- 33.1)
  Average prob (correct): 6.4% (+- 33.6)

# temperature: 0.15 top_k: 20 top_p: 0.8 min_p: 0.00
# llama.cpp vulkan enable_thinking: false
Model: Qwen3.5-35B-A3B-UD-Q6_K_S (gbnf)
  Accuracy: 56.86% (642/1129)
  Accuracy (aligned): 63.06% (712/1129)
  Accuracy (stripped): 66.34% (749/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 4557.06
  Average duration: 8.20 s
  Average prob: 3.3% (+- 33.3)
  Average prob (incorrect): 0.6% (+- 25.8)
  Average prob (stripped): 8.1% (+- 34.3)
  Average prob (aligned): 8.8% (+- 34.4)
  Average prob (correct): 11.8% (+- 34.3)

Model: Gemini 2.5 Pro (high) # reasoning_effort: high
  Accuracy: 55.18% (623/1129)
  Accuracy (aligned): 60.67% (685/1129)
  Accuracy (stripped): 63.42% (716/1129)
  Error Rate: 0.00% (0/1129)

# temperature: 0.15 top_k: 64 top_p: 0.95
# llama.cpp vulkan --reasoning off
Model: gemma-4-31B-it-Q6_K.gguf (gbnf)
  Accuracy: 54.30% (613/1129)
  Accuracy (aligned): 64.57% (729/1129)
  Accuracy (stripped): 67.40% (761/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 5385.62
  Average duration: 23.21 s
  Average prob: 71.5% (+- 25.0)
  Average prob (incorrect): 56.8% (+- 30.9)
  Average prob (stripped): 79.8% (+- 20.7)
  Average prob (aligned): 80.3% (+- 20.2)
  Average prob (correct): 81.9% (+- 18.8)

# temperature: 0.15 top_k: default (40) top_p: default (0.95) min_p: 0.01
# llama.cpp vulkan
Model: Qwen3-Coder-Next-UD-Q6_K_XL (default)
  Accuracy: 53.32% (602/1129)
  Accuracy (aligned): 58.64% (662/1129)
  Accuracy (stripped): 61.74% (697/1129)
  Error Rate: 3.72% (42/1129)
  Average tokens: 4036.88
  Average duration: 52.53 s

Model: Gemini 2.5 Flash (none no_diff) # reasoning_effort: none
  Accuracy: 53.06% (599/1129)
  Accuracy (aligned): 63.24% (714/1129)
  Accuracy (stripped): 66.25% (748/1129)
  Error Rate: 3.28% (37/1129)
  Average tokens: 1036.06
  Average duration: 1.18 s

# temperature: 0.15 top_k: default (40) top_p: default (0.95) min_p: 0.01
# llama.cpp vulkan
Model: Devstral-Small-2-24B-Instruct-2512-UD-Q6_K_XL (no_diff)
  Accuracy: 53.06% (599/1129)
  Accuracy (aligned): 60.85% (687/1129)
  Accuracy (stripped): 63.86% (721/1129)
  Error Rate: 0.27% (3/1129)
  Average tokens: 963.94
  Average duration: 7.06 s

# temperature: 0.15 top_k: 20 top_p: 0.8 min_p: 0.00
# llama.cpp vulkan reasoning off
Model: Qwen3.5-9B-UD-Q8_K_XL (gbnf)
  Accuracy: 53.06% (599/1129)
  Accuracy (aligned): 56.60% (639/1129)
  Accuracy (stripped): 59.96% (677/1129)
  Error Rate: 0.09% (1/1129)
  Average tokens: 4561.30
  Average duration: 9.16 s
  Average prob: 1.2% (+- 23.1)
  Average prob (incorrect): 0.2% (+- 16.3)
  Average prob (stripped): 3.6% (+- 25.2)
  Average prob (aligned): 3.9% (+- 25.5)
  Average prob (correct): 4.5% (+- 25.8)

# temperature: 0.15 top_k: default (40) top_p: default (0.95) min_p: 0.01
# llama.cpp vulkan
Model: Qwen3-Coder-Next-UD-Q2_K_XL (default)
  Accuracy: 52.97% (598/1129)
  Accuracy (aligned): 59.61% (673/1129)
  Accuracy (stripped): 62.62% (707/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 4241.78
  Average duration: 8.52 s
  Average prob: 6.7% (+- 39.6)
  Average prob (incorrect): 1.6% (+- 31.2)
  Average prob (stripped): 15.6% (+- 39.3)
  Average prob (aligned): 17.3% (+- 39.1)
  Average prob (correct): 21.7% (+- 38.3)

# context: layout: system_message: [ prompt ] user_message: [ training, diff ]
Model: Gemini 2.5 Pro (low userctx) # reasoning_effort: low
  Accuracy: 52.44% (592/1129)
  Accuracy (aligned): 56.95% (643/1129)
  Accuracy (stripped): 59.70% (674/1129)
  Error Rate: 5.49% (62/1129)
  Average tokens: 6014.82
  Average duration: 9.68 s

# temperature: 0.15 top_k: 20 top_p: 0.8 min_p: 0.00
# llama.cpp vulkan reasoning off
Model: Qwen3.5-9B-UD-Q6_K_XL (gbnf)
  Accuracy: 51.99% (587/1129)
  Accuracy (aligned): 56.16% (634/1129)
  Accuracy (stripped): 59.61% (673/1129)
  Error Rate: 0.18% (2/1129)
  Average tokens: 4565.06
  Average duration: 7.81 s
  Average prob: 1.3% (+- 23.4)
  Average prob (incorrect): 0.2% (+- 16.7)
  Average prob (stripped): 3.9% (+- 25.5)
  Average prob (aligned): 4.2% (+- 25.8)
  Average prob (correct): 5.0% (+- 26.2)

# context: layout: system_message: [ prompt ] user_message: [ training, diff ]
Model: Gemini 2.5 Pro (low no_diff) # reasoning_effort: low
  Accuracy: 51.99% (587/1129)
  Accuracy (aligned): 55.36% (625/1129)
  Accuracy (stripped): 58.02% (655/1129)
  Error Rate: 2.92% (33/1129)
  Average tokens: 1931.27
  Average duration: 9.11 s

# temperature: 0.7 top_k: 20 top_p: 0.8 min_p: 0
# llama.cpp vulkan
Model: Qwen3-Coder-30B-A3B-Instruct-Q6_K (default)
  Accuracy: 49.69% (561/1129)
  Accuracy (aligned): 54.21% (612/1129)
  Accuracy (stripped): 56.78% (641/1129)
  Error Rate: 0.09% (1/1129)
  Average tokens: 4252.31
  Average duration: 9.18 s
  Average prob: 33.1% (+- 35.4)
  Average prob (incorrect): 16.3% (+- 40.7)
  Average prob (stripped): 56.7% (+- 27.4)
  Average prob (aligned): 58.0% (+- 27.2)
  Average prob (correct): 61.6% (+- 25.9)

Model: Gemini 2.5 Flash (none default) # reasoning_effort: none
  Accuracy: 49.60% (560/1129)
  Accuracy (aligned): 60.41% (682/1129)
  Accuracy (stripped): 63.42% (716/1129)
  Error Rate: 6.20% (70/1129)
  Average tokens: 5069.04
  Average duration: 1.15 s

Model: nemotron-super-120b-bf16 (ebnf) # enable_thinking: false
  Accuracy: 49.42% (558/1129)
  Accuracy (aligned): 53.94% (609/1129)
  Accuracy (stripped): 58.28% (658/1129)
  Error Rate: 2.39% (27/1129)
  Average input tokens: 4500.75
  Average output tokens: 168.21

# context: layout: system_message: [ prompt ] user_message: [ training, diff ]
Model: Gemini 2.5 Flash (low no_diff userctx) # reasoning_effort low
  Accuracy: 48.72% (550/1129)
  Accuracy (aligned): 58.19% (657/1129)
  Accuracy (stripped): 62.00% (700/1129)
  Error Rate: 2.66% (30/1129)
  Average tokens: 1916.70
  Average duration: 4.62 s

# temperature: 0.7 top_k: 20 top_p: 0.8 min_p: 0
# llama.cpp vulkan
Model: Qwen3-Coder-30B-A3B-Instruct-Q6_K (no_diff)
  Accuracy: 46.94% (530/1129)
  Accuracy (aligned): 51.02% (576/1129)
  Accuracy (stripped): 53.76% (607/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 904.89
  Average duration: 4.37 s
  Average prob: 37.1% (+- 35.1)
  Average prob (incorrect): 24.0% (+- 39.1)
  Average prob (stripped): 53.8% (+- 29.1)
  Average prob (aligned): 57.3% (+- 27.9)
  Average prob (correct): 62.6% (+- 26.5)

# temperature: 0.15 top_k: 64 top_p: 0.95
# llama.cpp vulkan --reasoning off
Model: gemma-4-26B-A4B-it-UD-Q6_K_XL.gguf (gbnf)
  Accuracy: 43.93% (496/1129)
  Accuracy (aligned): 60.41% (682/1129)
  Accuracy (stripped): 63.42% (716/1129)
  Error Rate: 0.00% (0/1129)
  Average tokens: 5390.44
  Average duration: 8.48 s
  Average prob: 45.4% (+- 33.2)
  Average prob (incorrect): 24.8% (+- 38.6)
  Average prob (stripped): 64.3% (+- 27.3)
  Average prob (aligned): 64.2% (+- 27.4)
  Average prob (correct): 71.9% (+- 24.7)

# context: layout: system_message: [ prompt ] user_message: [ training, diff ]
Model: Gemini 2.5 Flash (low userctx) # reasoning_effort: low
  Accuracy: 42.52% (480/1129)
  Accuracy (aligned): 52.70% (595/1129)
  Accuracy (stripped): 55.98% (632/1129)
  Error Rate: 13.82% (156/1129)
  Average tokens: 5942.75
  Average duration: 4.22 s

# if Beam 0 is wrong, Beam 1 is right 10.54% of the time
Model: Patchpal AI 7B (#1)
  Accuracy: 9.21% (104/1129)
  Accuracy (aligned): 23.91% (270/1129) # might be duplicate with other beams
  Accuracy (stripped): 30.29% (342/1129) # might be duplicate with other beams
  Error Rate: 0.00% (0/1129)
  Average duration: 10.90 s
  Average prob: 59.7% (+- 20.6)
  Average prob (incorrect): 60.9% (+- 20.1)
  Average prob (stripped): 56.9% (+- 21.7)
  Average prob (aligned): 55.1% (+- 23.1)
  Average prob (correct): 73.0% (+- 14.1)

Model: Gemini 2.5 Flash (low default) # reasoning_effort: low
  Accuracy: 7.97% (90/1129)
  Accuracy (aligned): 9.57% (108/1129)
  Accuracy (stripped): 10.27% (116/1129)
  Error Rate: 85.56% (966/1129) # default layout fails with Gemini thinking mode
  Average tokens: 3719.80
  Average duration: 0.51 s

# this is comparable to Patchpal AI #1
Model: Qwen3-Coder-30B-A3B-Instruct (no_diff#1) # perplexity beam #1
  Accuracy: 7.71% (87/1129)
  Accuracy (aligned): 11.87% (134/1129) # might be duplicate with other beams
  Accuracy (stripped): 16.56% (187/1129) # might be duplicate with other beams
  Error Rate: 0.18% (2/1129)
  Average tokens: 910.68
  Average duration: 1.17 s # kvcached

# if Beam 0 and Beam 1 are wrong, Beam 2 is right 3.37% of the time
Model: Patchpal AI 7B (#2)
  Accuracy: 3.10% (35/1129)
  Accuracy (aligned): 18.60% (210/1129) # might be duplicate with other beams
  Accuracy (stripped): 26.40% (298/1129) # might be duplicate with other beams
  Error Rate: 0.09% (1/1129)
  Average duration: 10.89 s
  Average prob: 48.6% (+- 21.9)
  Average prob (incorrect): 50.3% (+- 21.6)
  Average prob (stripped): 44.1% (+- 22.7)
  Average prob (aligned): 41.5% (+- 24.2)
  Average prob (correct): 66.0% (+- 17.2)

# this is comparable to Patchpal AI #2
Model: Qwen3-Coder-30B-A3B-Instruct (default#2) # perplexity beam #2
  Accuracy: 1.95% (22/1129)
  Accuracy (aligned): 6.91% (78/1129) # might be duplicate with other beams
  Accuracy (stripped): 11.87% (134/1129) # might be duplicate with other beams
  Error Rate: 0.09% (1/1129)
  Average tokens: 913.69
  Average duration: 1.18 s # kvcached
```

---

## 🎥 Demo

> ![synthmerge-demo backtest_stable](https://gitlab.com/aarcange/synthmerge-assets/-/raw/main/synthmerge-demo-0.1.27-backtest_stable.webm)
> ![synthmerge-demo vibe](https://gitlab.com/aarcange/synthmerge-assets/-/raw/main/synthmerge-demo-0.1.27-vibe.webm)
> ![synthmerge-demo](https://gitlab.com/aarcange/synthmerge-assets/-/raw/main/synthmerge-demo-0.1.8.webm)
> ![synthmerge-demo with ripgrep-edit](https://gitlab.com/aarcange/synthmerge-assets/-/raw/main/synthmerge-demo-0.1.8-ripgrep-edit.webm)
> ![synthmerge-demo with vim](https://gitlab.com/aarcange/synthmerge-assets/-/raw/main/synthmerge-demo-0.1.8-vim.webm)

---

## License

[![License: GPL-3.0-or-later](https://img.shields.io/badge/License-GPL--3.0--or--later-blue.svg)](https://www.gnu.org/licenses/gpl-3.0.html) ![OR](https://img.shields.io/badge/OR-lightgrey.svg) [![License: AGPL-3.0-or-later](https://img.shields.io/badge/License-AGPL--3.0--or--later-blue.svg)](https://www.gnu.org/licenses/agpl-3.0.html)
