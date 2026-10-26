rm -r data
printf 'sample,ph,moisture,nitrogen\nS01-1,6.4,0.31,0.12\nS01-2,6.2,0.29,0.14\n' > data
wait_for_verdict
