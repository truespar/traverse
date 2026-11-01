package com.truespar.traverse.http;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;

@JsonIgnoreProperties(ignoreUnknown = true)
public record UploadResult(boolean ok, String name, String file, long bytes) {}
