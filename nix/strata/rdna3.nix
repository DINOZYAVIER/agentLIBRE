# The RDNA3 fork of Strata on one RX 7900 XTX (AGL-086, AGL-089): its 1-GPU
# settings of the UD-IQ4_XS (rdna3/configs/iq4-1gpu.json at 8b01b0c8) with what
# AGL-086 measured on a 62 GB machine whose desktop is on the card: no draft
# vocabulary subset of its own (D6), no conversation cache (D7), 2 GB of VRAM
# kept (the desktop), 44 GiB of experts in RAM (the engine takes all the free
# RAM else) and the prompt read in chunks of 16384 tokens (AGL-089 D26: with 47
# and 32768 an 84K prompt read ~4 GB from the drive and left 2 GiB of RAM; with
# these ~0.4 GB and 6 GiB, the prompt 3-7 % slower). agl fills the paths and the
# hipBLASLt tuning.
{
  strata = "rdna3";
  args = [
    "--prefill"
    "16384"
    "--spec"
    "4"
    "--spec-min-p"
    "0.5"
    "--kv"
    "int8"
    "--ple-io"
    "direct"
    "--stats"
    "--expert-cache"
    "auto"
    "--ple-row-cache"
    "1048576"
    "--conversation-cache-mib"
    "0"
    "--conversation-cache-slots"
    "8"
    "--kv-resident"
    "32768"
    "--pcie-frac"
    "0"
    "--vram-reserve-mib"
    "2048"
    "--pool-workers"
    "12"
    "--mtp-q4"
    "all"
    "--mtp-window"
    "4096"
    "--adapt-every"
    "1"
    "--adapt-swaps"
    "32"
    "--resident-budget-gib"
    "44"
  ];
  env = {
    STRATA_RDNA_MMVQ_WAVE = "3";
    STRATA_PEER_TRANSPORT = "host";
    STRATA_HIP_WMMA = "1";
    STRATA_SELECT_WMMA = "1";
    STRATA_SH_STREAM = "0";
    STRATA_PREFILL_AUTO_MAX = "32768";
    STRATA_PREFILL_MMQ_TILE_ROWS = "64";
    STRATA_GR_RDNA = "1";
    STRATA_GR_RDNA_Q8 = "1";
    STRATA_RDNA_Q6K = "1";
    STRATA_GR_RDNA_A8 = "1";
    STRATA_QFUSE = "1";
    STRATA_COMBINE_CPU = "1";
    STRATA_VERIFY_QDEDUP = "1";
    STRATA_WAIT_FUSE = "1";
    STRATA_ROUTE_PUBLISH = "1";
    STRATA_GR_INJ_UP = "1";
    STRATA_PF_FUSED = "1";
    STRATA_STAGER_RING = "64";
    STRATA_PREFILL_CPU_SHARE = "0.6";
    STRATA_PREFILL_CPU_SHARE_MAX = "5120";
    GPU_MAX_HW_QUEUES = "6";
    STRATA_GR_FUSED = "1";
    STRATA_GDN_HEAD = "1";
    STRATA_GDN_CHUNKED = "1";
    STRATA_GDN_CHUNK_GDV = "0";
    STRATA_PA_KLDS = "1";
    STRATA_GDN_PP = "1";
    STRATA_GDN_CONVL2 = "1";
    STRATA_GDN_NOY = "1";
    STRATA_PF_GEMM = "1";
    STRATA_PF_PAD = "1";
    STRATA_HC_UPMIX = "1";
    STRATA_PF_HCDOWN = "1";
    STRATA_FUSE_CONVAB = "1";
    STRATA_HEAD_MIX_MULTI = "1";
    STRATA_PA_FAST = "1";
    STRATA_PF_GROUP_RES = "1";
    STRATA_PF_F16 = "2";
    STRATA_PREFILL_RING = "192";
    STRATA_KV_HOST_DMA = "1";
    STRATA_ADAPT_OVERLAP = "1";
    STRATA_PF_FUSED_KQ = "1";
    STRATA_TOPK_SPLIT_MIN = "98304";
    STRATA_QSA_ROWS = "1";
    STRATA_ENGINE_NICE = "0";
  };
}
