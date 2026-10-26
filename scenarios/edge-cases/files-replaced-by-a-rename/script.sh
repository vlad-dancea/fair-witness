# An editor that saves safely writes the new text to a temporary file and renames it over the
# old one.
printf '# Field notes\n\n- Site 3 was waterlogged.\n- Site 7 is behind a new fence.\n' \
  > notes/.field-notes.md.swx-save
mv notes/.field-notes.md.swx-save notes/field-notes.md
wait_for_verdict

# sed -i does the same with a temporary file of its own.
sed -i 's/^S01-1,6.4,/S01-1,6.5,/' data/campaign-2026-04.csv
wait_for_verdict

# A file manager replaces a file with one moved in from elsewhere.
outside=$(mktemp -d)
cp data/campaign-2026-04.csv "$outside/"
printf 'S03-1,6.8,0.27,0.11\n' >> "$outside/campaign-2026-04.csv"
mv "$outside/campaign-2026-04.csv" data/campaign-2026-04.csv
wait_for_verdict
