---
title: Pricing
url: https://platform.claude.com/docs/en/about-claude/pricing
description: Learn about Anthropic's pricing structure for models and features
---

This page provides detailed pricing information for Anthropic's models and features. All prices are in USD.

For the most current pricing information, visit [claude.com/pricing](https://claude.com/pricing).

## Model pricing

The following table shows pricing for all Claude models:

| Model                                                                                                                                | Base input tokens     | 5m cache writes | 1h cache writes | Cache hits and refreshes | Output tokens          |
| :----------------------------------------------------------------------------------------------------------------------------------- | :-------------------- | :-------------- | :-------------- | :----------------------- | :--------------------- |
| Claude Mythos 5.1 ([limited availability](https://support.claude.com/en/articles/14604842))                                          | $10 / MTok            | $12.50 / MTok   | $20 / MTok      | $0.25 / MTok<sup>1</sup> | $50 / MTok             |
| Claude Opus 5.5                                                                                                                      | $4 / MTok             | $5 / MTok       | $8 / MTok       | $0.20 / MTok<sup>2</sup> | $20 / MTok             |
| Claude Opus 4.1 ([retired, except on Bedrock and Google Cloud](https://platform.claude.com/docs/en/about-claude/model-deprecations)) | $15 / MTok            | $18.75 / MTok   | $30 / MTok      | $1.50 / MTok             | $75 / MTok             |
| Claude Sonnet 4.5                                                                                                                    | $3 / MTok             | $3.75 / MTok    | $6 / MTok       | $0.30 / MTok             | $15 / MTok             |
| Claude Haiku 5.5 (for prompts up to 100,000 tokens)                                                                                  | $0.10 / MTok          | $0.125 / MTok   | $0.20 / MTok    | $0.01 / MTok             | $0.50 / MTok           |
| Claude Haiku 5.5 (for prompts over 100,000 tokens)                                                                                   | $0.50 / MTok          | $0.625 / MTok   | $1 / MTok       | $0.05 / MTok             | $2.50 / MTok           |
| Claude Haiku 4.5                                                                                                                     | $1 / MTok             | $1.25 / MTok    | $2 / MTok       | $0.10 / MTok             | $5 / MTok              |

*<sup>1 Cache hits and refreshes on Claude Fable 5.1 and Claude Mythos 5.1 are priced at 0.025x the base input price.</sup>*

*<sup>2 Cache hits and refreshes on Claude Opus 5.5 and Claude Sonnet 5.5 are priced at 0.05x the base input price.</sup>*

*<sup>All other models use the standard 0.1x multiplier.</sup>*

*<sup>3 The $2/$10 per million input/output token pricing for Claude Sonnet 5, announced at launch as introductory pricing through August 31, 2026, is now the standard price. The previously scheduled increase to $3/$15 per million input/output tokens on September 1, 2026 will not occur.</sup>*

## Cloud platform pricing


### Batch processing

The Batch API allows asynchronous processing of large volumes of requests with a 50% discount on both input and output tokens.

| Model                                                                                                                                | Batch input  | Batch output  |
| :----------------------------------------------------------------------------------------------------------------------------------- | :----------- | :------------ |
| Claude Haiku 5.5 (for prompts up to 100,000 tokens)                                                                                  | $0.05 / MTok | $0.25 / MTok  |
| Claude Haiku 5.5 (for prompts over 100,000 tokens)                                                                                   | $0.25 / MTok | $1.25 / MTok  |

