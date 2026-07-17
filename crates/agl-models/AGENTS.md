# agl-models

This crate owns model catalog, acquisition, install records, host-resource
planning, and safe model lifecycle operations. It must not load llama.cpp,
render CLI output, or persist Hugging Face credentials.

Use the async `hf-hub` client only behind `ModelDownloadWorker`. Do not enable
or use `HFClientSync`, shell out to download tools, scan arbitrary model
directories, or infer bindings from filenames.
