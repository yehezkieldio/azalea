# Legal Notice

> This document is for informational purposes only and does not constitute legal advice. It describes legal and platform-policy risks that may arise from deploying or using Azalea. Consult a qualified professional for advice specific to your situation.

## What Azalea Does

Azalea is a Discord bot that retrieves media from X (formerly Twitter) and uploads it to a Discord channel. When a user invokes `/media <url>`, Azalea resolves the media at the given URL, downloads it, transcodes it if necessary to fit within Discord's upload limit, and posts it as a native attachment.

Because this process automates the retrieval and redistribution of third-party content, operating Azalea carries legal and policy risks for the operator.

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

Azalea is intended for educational and experimental use and for convenience within a private server. It is not intended to facilitate copyright infringement, circumvent platform rules, or enable systematic scraping, archiving, or redistribution of content.

## Third-Party Code Attribution

Azalea (MIT OR Apache-2.0) does not include source code from any AGPL-3.0-licensed project. Where research into another project informed a design decision or a fix, that influence is documented here and in the relevant source comments, distinguishing facts/techniques (not copyrightable) from original expression (which is not copied).

- **[imputnet/cobalt](https://github.com/imputnet/cobalt)** (AGPL-3.0-only) — a media downloader whose `api/` package was reviewed for ideas applicable to Azalea's own Twitter/X resolver (`azalea-core/src/pipeline/resolve.rs`):
  - **Used, independently reimplemented**: detection of a Twitter video-muxer bug (Nov-Dec 2023) that produced broken containers, via decoding the affected media's Snowflake-ID timestamp. cobalt discovered and documented this bug and its date window; Azalea's `twitter_container_bug_window` function decodes the same publicly-documented Snowflake ID format (a technique used by many platforms, not original to cobalt) independently, in different code structure, to apply the same fix (force a remux instead of a byte-for-byte pass-through) in Azalea's own pipeline.
  - **Investigated and rejected**: a resolver hitting Twitter's GraphQL `TweetDetail` endpoint directly, modeled on cobalt's `twitter.js`. This was implemented, live-tested, found to be blocked by Twitter's TLS-fingerprinting (consistent with a maintainer-documented cobalt issue, [#573](https://github.com/imputnet/cobalt/issues/573)), and removed rather than pursuing TLS-impersonation to work around it. No code from this attempt remains; see the removal's rationale in `resolve.rs`'s module docs.

If any future change incorporates a nontrivial, original portion of AGPL-3.0 (or other copyleft) source rather than an independently-written reimplementation of a documented technique, that code must be either rewritten from scratch, isolated and relicensed under compatible terms with the upstream author's permission, or not merged — maintainers should treat this as a hard gate, not a documentation afterthought.
