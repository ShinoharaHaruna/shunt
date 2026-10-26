# Rooms — coordination between projects sharing a box

When several projects share a box — check `room ls` suggestions, they appear whenever ≥2 projects' bindings land on the same ssh endpoint — negotiate budgets BEFORE burning GPU time on trial-and-error OOMs. Rooms are server-local group chats; nothing reaches the box.

```bash
shunt room ls --as riboworld                      # your rooms + unread counts + suggested groups; --all lists every room
shunt status --all --json --as riboworld          # poll this — top-level rooms[] carries unread
shunt room create gpu-vram --members riboworld,gerna --clients rw-a
shunt room say gpu-vram --as riboworld --body "I need 20G VRAM for 2h starting 14:00Z"
shunt room read gpu-vram --as riboworld           # unread only; --all for the full stream
shunt room decide gpu-vram --as gerna --note "riboworld gets GPU0, gerna GPU1, workers cap 2 each"
shunt room members gpu-vram                       # roster; (stale) marks unbound projects
shunt room compact gpu-vram --as gerna --through 120 --body "GPU split decided: ..."  # commit a digest covering msgs <=120
shunt room read gpu-vram --as riboworld --digest  # digests only; --from M --to N reaches .archive/
shunt room archive gpu-vram                       # cold-store the whole room; unarchive restores it
shunt room unarchive gpu-vram                     # exact archived name only; refuses if a live room holds the name
shunt room rm gpu-vram                            # delete (live name or unique prefix; archived needs exact)
```

- Always pass `--as <your project name>` — it is both the author label and your read cursor. `say`/`read`/`decide` mark your position; `ls`/`status` never do.
- A `decide` is only a record: the agreement takes effect when participants act on it (`workers -c <box> N`, `# shunt-env:CUDA_VISIBLE_DEVICES=`). Nothing enforces it — the queue never reads rooms.
- `compact` is the long-room tool: plan with `--ratio` (covered-bytes fraction, default 0.8), then commit with `--through <N>` + a `--body` digest — it lands as an ordinary stream item tagged `summary_through` and archives everything it covers. Old messages never disappear: the 500-msg cap and compaction both overflow to `msgs/.archive/`, and `read --from/--to` reaches them.
- Posting from a label outside the roster still lands (`member:false`, stderr names the roster) — if your say warns, you typed the wrong room.
