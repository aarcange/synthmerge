#!/usr/bin/env python3
#
# SPDX-License-Identifier: GPL-3.0-or-later OR AGPL-3.0-or-later
# Copyright (C) 2026  Red Hat, Inc.

import matplotlib.pyplot as plt
import re
import sys
import os

# 1. Data Definition
# Format: List of model names
models = [
    "DeepSeek-V4.1-Flash-1017 (high, temp 0) {0.4.1}",
    "DeepSeek-V4.1-Flash-1017 (max, temp 0) {0.4.1}",
    "Gemini 3.8 Flash (medium) {0.3.16}",
    "Gemini 3.7 Flash (medium)",
    "Gemini 3.6 Flash (medium)",
    "Gemini 3.5 Flash (medium)",
    "Gemini 3 Flash (none)",
    "Gemini 2.5 Flash (none)",
    # "Gemini 2.5 Flash (none no_diff)",
    "Gemini 2.5 Flash (low userctx)",
    # "Gemini 2.5 Flash (low)",
    "gpt-6.1-sol (xhigh) {0.3.16}",
    "gpt-6.1-sol (high) {0.3.16}",
    "gpt-6-astra (xhigh) {0.3.16}",
    "gpt-5.6-sol (xhigh) {0.3.16}",
    "gpt-5.6-sol (high)",
    "gpt-5.6-terra (medium)",
    "gpt-5.6-luna (medium)",
    "gpt-5.5 (high)",
    "gpt-5.5 (medium)",
    "Claude Opus 5.5 (adaptive) {0.3.16}",
    "Claude Opus 5 (adaptive) {0.3.16}",
    "Claude Opus 4.8 (adaptive)",
    "Claude Opus 4.7 (adaptive)",
    "Claude Opus 4.6 (adaptive)",
    "Gemini 3.1 Pro (high)",
    "Gemini 2.5 Pro (low userctx)",
    # "Gemini 2.5 Pro (high)",
    "Patchpal AI 7B #0",
    "Claude Sonnet 5 (adaptive)",
    "Claude Sonnet 4.6 (default)",
    "Claude Sonnet 4.5 (default)",
    "Claude Sonnet 4.0 (default)",
    "Qwen3.8-27B-UD-Q6_K_XL (gbnf)",
    "Qwen3.6-35B-A3B-UD-Q6_K_XL (gbnf)",
    "Qwen3.6-27B-UD-Q6_K_XL (gbnf)",
    "Qwen3.5-27B-UD-Q6_K_XL (gbnf)",
    "Qwen3.5-9B-UD-Q8_K_XL (gbnf)",
    # "Qwen3-Coder-30B-A3B-Instruct-Q6_K (default)",
    # "Qwen3-Coder-Next-UD-Q6_K_XL (default)",
    "GLM 5.3 (high) {0.3.16}",
    "Devstral-Small-2-24B-Instruct-2512-UD-Q6_K_XL (default)",
]


def get_data(model_list):
    """
    Search README.md in the parent directory for accuracy, accuracy aligned, accuracy stripped, and error rate values
    for the given list of models and return the data in the original format.
    """
    # Determine the path to README.md
    script_dir = os.path.dirname(os.path.abspath(sys.argv[0]))
    parent_dir = os.path.dirname(script_dir)
    readme_path = os.path.join(parent_dir, "README.md")

    if not os.path.exists(readme_path):
        raise FileNotFoundError(f"README.md not found at {readme_path}")

    with open(readme_path, "r", encoding="utf-8") as f:
        readme_content = f.read()

    data = []
    for model_name in model_list:
        # Escape special regex characters in model name
        escaped_name = re.escape(model_name)
        # Pattern to match the model line in README.md
        # Example: "Model: Claude Opus 4.6 (default)"
        # We look for the model name followed by optional parenthetical info
        pattern = rf"Model:\s*{escaped_name}(?:\s+#.*?)?\n.*?Accuracy:\s*([\d.]+)%.*?Accuracy\s*\(aligned\):\s*([\d.]+)%.*?Accuracy\s*\(stripped\):\s*([\d.]+)%.*?Error Rate:\s*([\d.]+)%"
        matches = list(re.finditer(pattern, readme_content, re.DOTALL))

        if len(matches) == 0:
            raise ValueError(f"Could not find accuracy data for model: {model_name}")
        if len(matches) > 1:
            raise ValueError(f"Found multiple matches for model: {model_name}")

        match = matches[0]
        accuracy = float(match.group(1))
        accuracy_aligned = float(match.group(2))
        accuracy_stripped = float(match.group(3))
        error_rate = float(match.group(4))
        if model_name.count("(") == 1:
            model_name = model_name.replace(" (", "\n(")
        model_name = model_name.replace(" +", "\n+")
        data.append(
            (
                model_name,
                accuracy,
                accuracy_aligned,
                accuracy_stripped,
                error_rate,
            )
        )

    return data


# 2. Preprocessing: Sort by Accuracy then Accuracy Aligned then Accuracy Stripped descending
data = get_data(models)
data.sort(key=lambda x: (x[1], x[2], x[3]), reverse=True)

# Extract lists for plotting
models = [d[0] for d in data]
accuracy = [d[1] for d in data]
accuracy_aligned = [d[2] for d in data]
accuracy_stripped = [d[3] for d in data]
error_rate = [d[4] for d in data]

# 3. Setup Plot
plt.style.use("seaborn-v0_8-dark")
fig, ax = plt.subplots(figsize=(16, 9))

# Define positions for grouped bars
x = range(len(models))
bar_width = 0.2

# Plot bars
bars1 = ax.bar(
    [i - 3 * bar_width / 2 for i in x],
    accuracy,
    width=bar_width,
    label="Accuracy",
    color="#3498db",
    alpha=0.9,
    edgecolor="black",
)
bars2 = ax.bar(
    [i - bar_width / 2 for i in x],
    accuracy_aligned,
    width=bar_width,
    label="Accuracy (Aligned)",
    color="#9b59b6",
    alpha=0.9,
    edgecolor="black",
)
bars3 = ax.bar(
    [i + bar_width / 2 for i in x],
    accuracy_stripped,
    width=bar_width,
    label="Accuracy (Stripped)",
    color="#2ecc71",
    alpha=0.9,
    edgecolor="black",
)
bars4 = ax.bar(
    [i + 3 * bar_width / 2 for i in x],
    error_rate,
    width=bar_width,
    label="Error Rate",
    color="#e74c3c",
    alpha=0.9,
    edgecolor="black",
)

# 4. Customization
# ax.set_xlabel("Model", fontsize=12, fontweight="bold")
ax.set_ylabel("Percentage (%)", fontsize=12, fontweight="bold")
ax.set_title(
    'Model Performance: "Accuracy" vs. "Accuracy (Aligned)" vs "Accuracy (Stripped)"\nSorted by "Accuracy"',
    fontsize=14,
    fontweight="bold",
    pad=20,
)
ax.set_xticks(x)
ax.set_xticklabels(models, rotation=90, fontsize=9, ha="center")
ax.legend(loc="best", fontsize=11)
ax.set_ylim(0, 100)


# Add value labels on top of bars (optional, keeps it clean for many bars)
def add_labels(bars):
    for bar in bars:
        height = bar.get_height()
        ax.text(
            bar.get_x() - bar.get_width() / 2.5,
            height + 0.5,
            f"{height:.1f}",
            ha="center",
            va="bottom",
            fontsize=8,
            color="black",
        )


add_labels(bars1)
add_labels(bars2)
add_labels(bars3)
add_labels(bars4)

# Shared arrow properties so the arrows above the bars and the arrow
# in the legend are drawn identically
open_source_arrowprops = dict(
    arrowstyle="->",
    color="green",
    lw=2,
    mutation_scale=10,
    shrinkA=2,
    shrinkB=2,
)
# Arrow length in points (same as the annotation xytext offset)
open_source_arrow_length = 20


def is_open_source(model_name):
    """Check if the model is an open source model."""
    open_source_keywords = ["qwen", "deepseek", "glm", "devstral"]
    lower_name = model_name.lower()
    return any(keyword in lower_name for keyword in open_source_keywords)


# Mark open source models with a green arrow above the chart pointing down
open_source_indices = [i for i, m in enumerate(models) if is_open_source(m)]
if open_source_indices:
    for i in open_source_indices:
        max_height = max(
            accuracy[i], accuracy_aligned[i], accuracy_stripped[i], error_rate[i]
        )
        ax.annotate(
            "",
            xy=(i, max_height + 5),
            xytext=(0, open_source_arrow_length),
            textcoords="offset points",
            arrowprops=open_source_arrowprops,
            annotation_clip=False,
        )

# Add legend entry for open source models
from matplotlib.legend_handler import HandlerBase
from matplotlib.patches import FancyArrowPatch


class OpenSourceArrow:
    """Proxy artist for the open source legend entry."""


class HandlerOpenSourceArrow(HandlerBase):
    """Draw the same downward FancyArrowPatch used above the bars."""

    def create_artists(
        self, legend, orig_handle, xdescent, ydescent, width, height, fontsize, trans
    ):
        x = -xdescent + width / 2
        y_center = -ydescent + height / 2
        half = open_source_arrow_length / 2
        arrow = FancyArrowPatch(
            (x, y_center + half),
            (x, y_center - half),
            **open_source_arrowprops,
        )
        arrow.set_transform(trans)
        return [arrow]


handles, labels = ax.get_legend_handles_labels()
handles.insert(0, OpenSourceArrow())
labels.insert(0, "Open Weight Open Source")
ax.legend(
    handles=handles,
    labels=labels,
    loc="best",
    handler_map={OpenSourceArrow: HandlerOpenSourceArrow()},
)

# Adjust layout to prevent label cutoff
plt.tight_layout()

# 5. Display
plt.show()
