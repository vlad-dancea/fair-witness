# Five days of photos from six camera traps, one folder per camera and day. Each photo starts
# like a camera's JPEG and is padded to a camera's size without taking up the disk.
for camera in 1 2 3 4 5 6; do
  for day in 21 22 23 24 25; do
    folder="cameras/camera-$camera/2026-09-$day"
    mkdir -p "$folder"
    for photo in $(seq -w 1 15); do
      file="$folder/IMG_$camera$day$photo.JPG"
      printf '\377\330\377\341\000\042Exif\000\000' > "$file"
      truncate -s $((2400000 + (10#$photo * 7919 + camera * 104729) % 900000)) "$file"
    done
  done
done
wait_for_verdict
