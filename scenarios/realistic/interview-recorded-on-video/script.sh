# Grows a file the way `cp` writes it, keeping it open, at about 85 MB a second. Each step
# skips ahead in the open file and writes one byte there, so the file grows without taking up
# the disk. The file starts like the format it stands in for.
copy_in() {
  local file="$1" header="$2" size="$3" step
  exec 3> "$file"
  printf "$header" >&3
  local written=$(stat -c %s "$file")
  while ((written < size)); do
    step=$((size - written < 21000000 ? size - written : 21000000))
    # dd skips ahead from where the open file is, so by one byte less than the step.
    dd if=/dev/zero bs=1 count=1 seek=$((step - 1)) status=none >&3
    written=$((written + step))
    sleep 0.25
  done
  exec 3>&-
}

# The recorder's battery was empty, so the researcher recorded the interview with a camera and
# copies the clip from its SD card under the project's name for it.
copy_in interviews/recordings/interview-07-2026-10-20.mp4 '\000\000\000\030ftypmp42\000\000\000\000isommp42' 2147000000
wait_for_verdict

# Later they extract the audio with ffmpeg, which writes it over a few seconds.
copy_in interviews/recordings/interview-07-2026-10-20.m4a '\000\000\000\034ftypM4A \000\000\000\000M4A isom' 98000000
wait_for_verdict

# And delete the video.
rm interviews/recordings/interview-07-2026-10-20.mp4
wait_for_verdict
