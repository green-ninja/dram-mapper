# dram-mapper

  # Build
  
  cd ~/Documents/dram-mapper
  cargo build

  # Run (requires sudo for /proc/self/pagemap and huge pages)
  sudo ./target/debug/dram-mapper --fast             # ~2 min, full map
  sudo ./target/debug/dram-mapper --balanced          # ~10 min, more accurate
  sudo ./target/debug/dram-mapper --precise           # ~40 min, most accurate
  sudo ./target/debug/dram-mapper --fast --limit 8192 # test 8GB only
  sudo ./target/debug/dram-mapper --fast --verbose    # show all regions

  If huge pages get stuck (low available memory on next run):
  echo 0 | sudo tee /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages
  sleep 2 && cat /proc/meminfo | grep MemAvailable                                                                                                                                                                                                                                                                                                                                           
