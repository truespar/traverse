package com.truespar.traverse.http;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;
import java.util.List;

@JsonIgnoreProperties(ignoreUnknown = true)
public record ImportResult(
        @JsonProperty("total_statements") long totalStatements,
        long executed,
        List<Error> errors,
        QueryResponse.Stats stats,
        @JsonProperty("time_ms") double timeMs) {

    @JsonIgnoreProperties(ignoreUnknown = true)
    public record Error(String statement, String error) {}
}
