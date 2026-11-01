package com.truespar.traverse.http;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import tools.jackson.databind.JsonNode;

@JsonIgnoreProperties(ignoreUnknown = true)
public record TaskInfo(
        String id,
        String operation,
        String status,
        JsonNode result,
        String error,
        String createdAt,
        String finishedAt) {}
