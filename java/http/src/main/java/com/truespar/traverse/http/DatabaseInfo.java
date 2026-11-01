package com.truespar.traverse.http;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
import com.fasterxml.jackson.annotation.JsonProperty;
import java.util.List;

@JsonIgnoreProperties(ignoreUnknown = true)
public record DatabaseInfo(
        String name,
        @JsonProperty("default") boolean isDefault,
        boolean readOnly,
        List<String> aliases) {}
