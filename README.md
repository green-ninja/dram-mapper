# dram-mapper

  # Build
  
  cd ~/Documents/dram-mapper  
  cargo build  

  # Run (requires sudo for /proc/self/pagemap and huge pages)
  `sudo ./target/debug/dram-mapper --fast             # ~2 min, full map`  
  `sudo ./target/debug/dram-mapper --balanced          # ~10 min, more accurate`  
  `sudo ./target/debug/dram-mapper --precise           # ~40 min, most accurate`  
  `sudo ./target/debug/dram-mapper --fast --limit 8192 # test 8GB only`  
  `sudo ./target/debug/dram-mapper --fast --verbose    # show all regions`  

  If huge pages get stuck (low available memory on next run):
  `echo 0 | sudo tee /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages`  
  `sleep 2 && cat /proc/meminfo | grep MemAvailable`                                                                                                                                                                                                                                                                                                      <img width="1398" height="1336" alt="Screenshot From 2026-04-22 12-07-38" src="https://github.com/user-attachments/assets/adb6079b-21fc-4cfc-8031-c1284cd0dfc0" />
                                     
