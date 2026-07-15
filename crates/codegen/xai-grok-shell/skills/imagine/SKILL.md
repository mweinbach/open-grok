---
name: imagine
description: >
  How to use the image_gen and image_edit tool calls in Grok Build: when to
  build a visual with code instead of generating it, prompt-craft,
  reference-first handling of real people, factual grounding, and
  asset-consistency. Load this whenever generating or editing an image is on the
  table, i.e. when an image_gen or image_edit call is being considered or about
  to be made. Tool-usage-driven, not triggered by a user merely mentioning
  images.
metadata:
  short-description: "Prompting and workflow guidance for Imagine image tools"
---

# Imagine

Guidance for the two image tool calls in Grok Build:

- `image_gen` - generate a **new** image from a text prompt.
- `image_edit` - modify an **existing** image using a text prompt and source image.

Apply this whenever you're considering or about to call either tool.

## Build accurate visuals with code, not the image tools

1. **Image models are unreliable at exact text, numbers, and structure.** They can handle short text or a simple layout, but they often garble words, invent numbers, draw chart bars that match no data, or point diagram arrows nowhere, and the more that has to be exact, the worse they do. A detailed prompt doesn't make it dependable, and an `image_edit` pass usually won't fix it. So when a result needs specific text, data, or structure to be correct (charts from real numbers, labeled or technical diagrams, math explainers, tables, screens with real copy), construct the asset with code, where you control the exact content. Prefer HTML and CSS, which give much better layout, typography, and polish than Python plotting. When only the look matters (photos, illustrations, characters, scenes, decorative art), the image tools are the right choice. Which one fits depends on what the output needs to get right, not on how the request is worded.

## Verifying discrete accuracy (loop)

When the output must get specific text, numbers, data, or structure right, don't trust the first result - verify it in a loop:

1. Produce the result (generate, or per *Build accurate visuals with code*, construct it in code).
2. Inspect the actual output - use image understanding to read a generated image back (or check the rendered code) - and confirm every word, number, label, and structural detail matches the requirement, and that nothing overlaps, clips, or runs off-canvas.
3. If anything is wrong, fix and re-verify:
   - Garbled text, invented numbers, or broken layout from an image model? Don't just re-prompt - it will likely garble it again. Rebuild it with code.
   - Overlapping or clipped elements in code-built output? Re-lay-out with auto-layout (HTML/CSS) rather than nudging coordinates by hand.
   - Otherwise make one targeted edit.
4. Only finish when the discrete content is exactly correct. If it can't be made accurate, tell the user instead of shipping something wrong.

## Core Principles

1. **You own the prompt.** If the user gives a detailed prompt or asks you to use theirs, use it verbatim. Otherwise craft the final prompt: front-load the subject, give strong high-level direction for mood, composition, lighting, and style without over-specifying every detail, write natural prose rather than keyword tags, and describe positively instead of using negative prompts. For edits, describe only what changes. Target 2-5 sentences.
2. **Reference-first for real people.** Never use pure `image_gen` for a named real person or group, including face swaps, posters, cartoons, and cinematic or editorial depictions. Use `image_edit` with a real reference instead, and never produce non-consensual, sexualized, or minor-involving likenesses. See Real People and References for the procedure.
3. **Ground facts with search first.** If any part of the request depends on a real-world fact, identity, brand or product, place, event, or top/latest/current result, search the web before generating and put the actual verified details into the prompt. Don't rely on memory, and don't write vague placeholders like "the current president"; write the verified name.
4. **Reuse a base image for consistency.** When the same character, object, or setting must appear across multiple images, generate one base image first, then use it as the input to `image_edit` for every variation. Don't re-run `image_gen` from scratch for a recurring subject.
5. **Handle failures gracefully.** On a moderation or safety block, stop; don't retry and don't paraphrase the prompt to evade the filter. Tell the user it was blocked and offer a different direction. If a reference is weak or a result looks off-target, say so and ask for an upload or redirect rather than silently iterating.
6. **Plan multi-step workflows.** Sequence the steps; only parallelize generations that belong to the same step.
7. **Review at the end.** Confirm the generations you intended actually executed and match what was asked.
8. **Don't assume tool behavior.** Don't invent tool parameters, return values, or environment capabilities that aren't actually provided; verify rather than guess.

## Choosing the Tool

| Situation | Tool |
|-----------|------|
| New image, no source image | `image_gen` |
| Edit, restyle, recolor, add, remove, or extend an existing image | `image_edit` |
| Iterate on a previous result while keeping composition | `image_edit` |
| Named real person or group | `image_edit` with a real reference after a web search |
| Generic, invented, or non-factual subject from scratch | `image_gen` |

Rule of thumb: **no source image -> `image_gen`; source image -> `image_edit`.**

## `image_gen`

Generates a new image from a text prompt.

Inputs:

- `prompt` (required) - full description of the desired image.
- `aspect_ratio` - e.g. `1:1`, `16:9`, `9:16`, `4:3`, `3:4`, or `auto`.

Use for generic or invented subjects, or to create a base image you'll edit later. Not for named real people; see Reference-first for real people.

To produce multiple variations, make multiple `image_gen` calls with distinct prompts. The tool does not expose `n` or `count` parameters.

## `image_edit`

Transforms an existing image according to a prompt.

Inputs:

- `prompt` (required) - describe the desired transformation, and note what should stay the same.
- `image` (required) - one or more source/reference images as filesystem paths or `data:image/...;base64,...` URLs. Prefer a single clean reference for reliable results.
- `aspect_ratio` - optional; used for multi-image edits. Single-image edits preserve the input image aspect ratio.

Use to restyle, recolor, add or remove elements, preserve likeness, transfer style, remix, or iterate on a generated result.

To produce multiple variations, make multiple `image_edit` calls. The tool does not expose `n` or `count` parameters.

## Writing Strong Prompts

Describe, roughly in this order: **subject -> action/pose -> setting -> style -> composition -> lighting/mood -> key details.**

- Be specific and concrete; lead with the most important elements.
- State what to include rather than what to exclude.
- Use one coherent scene per prompt.
- Match `aspect_ratio` to the use case when using `image_gen`: `9:16` for phone/story, `16:9` for banner/video frame, `1:1` for avatar/icon.

## Real People and References

1. Search the web first to confirm identity, role, relationship, or event, even when it seems obvious.
2. Use a single strong reference with `image_edit`. A user-uploaded photo is best; otherwise use a high-quality found reference and cite the source. `image_edit` can take more than one reference, but one clean reference is more reliable.
3. If no suitable reference exists, ask the user to upload one rather than generating from a weak base.

## Video

> The video tools below may not exist - verify they're available before calling them; if they're not, the user cannot do video gen with Imagine.

Video starts from an image - there is no text-to-video tool. Default to `image_to_video`.

**Think in shots.** Build video as a planned sequence of short shots, not one long take:

1. **Plan the story as shots** - break the idea into distinct shots, one beat each.
2. **Favor frequent, short shots** - prefer more 6s shots over fewer long ones; more cuts keep it dynamic and interesting.
3. **Create each shot's source image** with `image_gen` (or a multi-image `image_edit` when a shot must combine references), keeping characters and settings consistent (Core Principle 4).
4. **Animate each shot with `image_to_video`** - the source becomes frame 1.

Use `reference_to_video` only if the user asks for it or a shot genuinely needs multiple references - and even then, prefer composing those references with a multi-image `image_edit` and animating the result with `image_to_video`.

Key behaviors:

- **Prompt-craft:** one short, vivid moment in present tense with a clear camera movement, in 1-2 sentences.
- **Minimal but interesting:** keep each shot to one clear subject and a single, simple motion or camera move. Avoid complex or multi-action animation (models handle it poorly); make the shot interesting through composition, lighting, and a strong moment, not busy motion.
- **Complex source image?** An intricate frame (busy geometry, fine detail, heavy reflections) warps when animated. If you must use it, keep the subject fixed and move only the camera (slow push-in, orbit, or parallax), or break it into tighter, simpler shots. For new shots, generate a simpler, animation-friendly base image up front instead of animating a busy one.
- **`image_to_video` animates from frame 1**, so stage the intended first frame with `image_gen`/`image_edit` first.
- **Aspect ratio:** set it on the source image (`image_gen` `aspect_ratio`); don't re-crop an existing video.
- **Duration:** 6s or 10s only (prefer 6s shots); round to the nearest.
- **Real people:** reference-first - drive the video from a verified reference image; never animate a named person without one.
- Don't loop the same clip unless asked.

**Assemble shots with FFmpeg** using stream copy so there's no quality loss: `ffmpeg -f concat ... -c copy` - never re-encode. Keep every shot at the same resolution and frame rate so the copy works.