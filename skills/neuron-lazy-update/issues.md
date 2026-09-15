# When something didn't work

Users will say "x didn't work" or "why is x like that?". Your job is to find out and answer. Filing
an issue is an **offer** you make at the end, only when it fits. It isn't the goal.

## 1. Research first

Before saying anything about bugs, find out whether it's already known or intended:

1. **Is it a known limit?** Check `docs/STATUS.md` (rough, planned), the README honesty table (gated
   or unsupported writes), and `SECURITY.md` (things neuron deliberately won't do). See
   [answers.md](answers.md) for where each lives.
2. **Is it expected behaviour?** Check the GDD for how the feature is meant to work, and
   `neuron.exe <command> --help` for the exact options.
3. **Is it the user's setup?** Common causes: Synapse still running, the device unplugged, a write
   that's volatile and reset on power-cycle, or neuron not running elevated (the native Chroma face
   only).
4. **Has someone already reported it?** Search the issues, open **and** closed:

   ```powershell
   $q = [uri]::EscapeDataString('repo:worflor/neuron is:issue <two or three keywords>')
   (Invoke-RestMethod "https://api.github.com/search/issues?q=$q" -Headers @{ 'User-Agent' = 'neuron-lazy-update' }).items |
     Select-Object number, state, title, html_url
   ```

   Or, if `gh` is installed: `gh issue list --repo worflor/neuron --state all --search "<keywords>"`.
   Try two different sets of keywords before concluding there's no match. If the search errors, use
   the `gh` form. If that's not available either, tell the user you couldn't check for duplicates,
   and don't guess.

Then **answer the user** with what you found, and name the source.

## 2. Offer, don't push

Offer to draft an issue only if all of these hold:

- it looks like a real bug, or a genuine gap, not a documented limit or a setup problem;
- there's no existing issue for it;
- it isn't a security problem. Those never go in a public issue; point the user to the private
  report link in `SECURITY.md`.

Make the offer **once**, in one sentence, after your answer. For example: *"This looks like a real
bug and nobody has reported it yet. Want me to draft an issue you can review?"* If they say no or
ignore it, drop it and don't bring it up again.

If an issue **already exists**, don't draft a new one. Give the user the link. If they want, they can
add a 👍, or comment with anything new they have, such as a different device or clearer steps.

If it's really a **question or an idea** rather than something broken, the Discussions tab is often a
better fit than an issue: https://github.com/worflor/neuron/discussions

## 3. Draft it (only after a yes)

Pick the template: `bug_report.md` for something broken, or `feature_request.md` for something
missing. Gather what applies, running only read-only commands:

| field | how to get it |
|---|---|
| neuron version | first line of `SOURCE.txt` in the install folder, or `neuron.exe --version` |
| Windows build | `cmd /c ver` |
| device + PID | `neuron.exe list` |
| steps, expected, actual | from the conversation, in the user's own terms |
| diagnostics | ask the user to run the bench on the SYSTEM page and paste it; you can't run it |
| crash log | `neuron-crash.log` in the config folder, if it exists; only the relevant lines |

### Make it good, cheaply

A good issue is one the maintainer can act on without writing back to ask. Usually the user's own
description is enough, so start from that. Add only what's missing, in this order, and stop as soon
as the issue can be acted on:

1. **Ask what only the user can answer, and nothing else.** At most two or three short questions,
   asked together in one message. The ones that matter most: *what exactly did you do, step by
   step?* · *what did you expect instead?* · *does it happen every time?* · *did it ever work?*
   Skip any the conversation already answers.
2. **If it's visual** (lighting looks wrong, a UI element is misplaced or clipped, an overlay
   misbehaves), ask for a screenshot, or a short clip if it moves. One image beats a paragraph.
   GitHub can't take the image from you, so tell the user to drag it into the issue form.
3. **Reproduce it, but only by reading.** If one read-only command shows the problem, such as
   `neuron.exe dpi` returning an unexpected value, run it and paste the output. Never change a
   setting to reproduce something unless the user agrees to it.
4. **One or two web searches, only when they add something triage needs.** Examples: Razer's own
   documentation of how the feature is supposed to behave, a known Windows issue that matches, or
   the page for the exact device model. Link the single most useful result. No search is fine;
   link-stuffing is not.

Don't pad. Two sharp sentences are better than a long report that repeats itself. If you've done
steps 1–4 and something's still unclear, write that down under *anything else* rather than guessing.

### Write it like this

- **Title:** what breaks, specifically. `DPI resets to 800 after sleep on Naga V2 Pro`, not
  `DPI bug`.
- **Body:** follow the template's headings. Keep what the user **saw** separate from what you
  **suspect**, and label the guesses as guesses. No filler, and no praise or apology.
- **One issue per problem.** If the user hit two things, that's two drafts, and only if they want
  both.
- **Remove personal details.** Replace `C:\Users\<name>` with `%USERPROFILE%`. Leave out serial
  numbers, emails, network names, and anything from the user's files that isn't needed.
- **Suggest labels.** Add one type label and one area label from
  [.github/LABELS.md](https://github.com/worflor/neuron/blob/main/.github/LABELS.md), for example
  `bug` and `area:lighting`.
- **End the body with this line**, so triage knows how it was written:
  `Drafted with an AI agent from the reporter's description; reviewed by the reporter before filing.`

**Show the user the whole draft** and let them change anything. File it only after they approve that
exact text.

## 4. File it

If `gh` is installed and logged in:

```powershell
gh issue create --repo worflor/neuron --title "<title>" --body-file <draft.md> --label bug --label needs-triage --label area:<area>
```

Labels only stick if the user has triage rights on the repo. For everyone else GitHub silently drops
them, which is fine, because triage adds them.

Otherwise, give the user this link and tell them to paste the draft body into the form:

`https://github.com/worflor/neuron/issues/new?template=bug_report.md&title=<url-encoded title>`

(Use `template=feature_request.md` for a missing feature.) The template applies its own intake
labels when they submit.

Afterwards, give the user the issue link, then go back to whatever they were doing.
