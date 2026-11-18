# Upstream Strata on one RDNA3 card (AGL-089): the arguments and environment
# of the human's working config of Strata v0.1.45 (AGL-070) on a 62 GB machine
# whose desktop is on the card: 47 GiB of experts in RAM, 2 GB of VRAM kept.
# agl fills the paths and the hipBLASLt tuning.
{
  strata = "upstream";
  args = [
    "--expert-cache"
    "auto"
    "--prefill"
    "auto"
    "--spec"
    "4"
    "--spec-min-p"
    "0.5"
    "--kv"
    "int8"
    "--resident-budget-gib"
    "47"
    "--vram-reserve-mib"
    "2048"
  ];
  env = {
    GPU_PINNED_MIN_XFER_SIZE = "1048576";
    STRATA_RESIDENT_PIN = "0";
    HSA_USERPTR_FOR_PAGED_MEM = "0";
  };
}
