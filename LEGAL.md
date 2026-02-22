# Legal Notice

> This document is for informational purposes only and does not constitute legal advice. It describes legal and platform-policy risks that may arise from deploying or using Azalea. Consult a qualified professional for advice specific to your situation.

## What Azalea Does

Azalea is a Discord bot that retrieves media from X (formerly Twitter) and uploads it to a Discord channel. When a user invokes `/media <url>`, Azalea resolves the media at the given URL, downloads it, transcodes it if necessary to fit within Discord's upload limit, and posts it as a native attachment.

Because this process automates the retrieval and redistribution of third-party content, operating Azalea carries legal and policy risks for the operator.

## Repository License

Azalea is dual-licensed under the MIT License or the Apache License 2.0, at your option. You may use, copy, modify, and distribute this software under the terms of either license. See [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE) for the full license texts.

This file is a risk notice. It does not alter or replace those license terms.

## Limitation of Liability

Azalea is provided as-is, without warranty of any kind, to the maximum extent permitted by applicable law. The maintainers and contributors disclaim all liability for misuse of the software and for any claims, damages, or losses arising from your deployment or use of Azalea. You are solely responsible for ensuring your use complies with applicable law and any third-party terms or policies.

## Third-Party Terms and Policies

Operating Azalea may implicate the terms of the following services:

- X Terms of Service: https://x.com/en/tos
- X Developer Agreement: https://developer.x.com/en/developer-terms
- X Developer Policy: https://developer.x.com/en/developer-policy
- Discord Terms of Service: https://discord.com/terms
- Discord Community Guidelines: https://discord.com/guidelines

These documents are subject to change, and enforcement practices vary over time.

## Risk Areas

### X Terms of Service and Developer Policy

Azalea's core behavior — retrieving media from an X URL and redistributing it elsewhere — may be treated as automated access or automated extraction from X infrastructure. X's terms restrict automated access, scraping, and use of non-official interfaces. Violations can result in account action, IP blocking, rate limiting, or legal escalation.

Deployments relying on unofficial third-party resolvers carry higher policy risk than those using official, documented interfaces. High-volume and systematic access patterns increase enforcement exposure.

### Copyright and Related Rights

Media published on X is commonly protected by copyright owned by the creator or other rightsholders. Downloading and re-uploading that media creates new copies and new distribution pathways, which may implicate copyright law depending on jurisdiction and context.

Re-uploading content in full, primarily for convenience, is unlikely to qualify for fair use or equivalent exceptions in most jurisdictions. Rightsholders may issue complaints or takedown requests.

### Discord Content Policy and Account Risk

Azalea uploads content to Discord. You must comply with Discord's rules for bots, API usage, and content sharing. Discord's terms assign responsibility to operators and users for ensuring they hold appropriate rights to the content they upload. Violations can result in content removal or enforcement action against the server or bot account.

### Computer Access and Anti-Abuse Law

Beyond breach of contract, some jurisdictions have statutes addressing unauthorized automated access, circumvention of technical measures, or computer misuse. Applicability depends on what is accessed, how, at what scale, and what technical controls are in place.

### Privacy, Logging, and Data Retention

Depending on configuration, Azalea may process or retain user identifiers, message content, URLs, and associated metadata. If you operate an instance, you are responsible for determining what is logged, how long it is retained, and who can access it. Data protection obligations may apply depending on your jurisdiction and the identities of the users you serve.

## Third-Party Dependencies

Azalea depends on external tools and services for URL resolution, media retrieval, and transcoding. These dependencies may change behavior, be blocked, or introduce additional policy exposure. Using encrypted connections and restricting outbound network access improves security posture, but does not grant legal authorization to access or redistribute content.

## Project Intent

Azalea is intended for educational and experimental use and for convenience within private community chat. It is not intended to facilitate copyright infringement, circumvent platform rules, or enable systematic scraping, archiving, or redistribution of content.
