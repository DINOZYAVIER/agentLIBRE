# DinoBot on the default model with all its context (AGL-050, D19): 25.1 of
# 25.8 GB of VRAM with a desktop, so 24 GB and little else on the card. The
# drafter's KV cache in q8 and the smaller physical batch make the room: with
# the usual batch the VRAM is so full that it runs three times slower.
{ agl, ... }:
{
  policy = agl.policies.agentLIBRE;
  model = agl.models."qwen3.8-27b-iq4_xs" // {
    context = 262144;
    args = [ "-ngl" "999" "-fa" "on" "-ctk" "q8_0" "-ctv" "q8_0" "-ctkd" "q8_0" "-ctvd" "q8_0" "-ub" "256" ];
  };
}
