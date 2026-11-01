package com.truespar.traverse.http;

import com.fasterxml.jackson.annotation.JsonIgnoreProperties;

@JsonIgnoreProperties(ignoreUnknown = true)
public record AliasInfo(String alias, String database) {}
