package com.truespar.traverse.http;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;

/**
 * A file in the server's data directory.
 *
 * @param kind "database" for .tvdb files, "model" for .tvmodel trained models
 */
@JsonIgnoreProperties(ignoreUnknown = true)
public record TvdbFileInfo(
        String name,
        String file,
        long bytes,
        boolean loaded,
        String kind) {}
