Feature: Literals6 - String

  Scenario: [1] Return a single-quoted empty string
    Given any graph
    When executing query:
      """
      RETURN '' AS literal
      """
    Then the result should be, in any order:
      | literal |
      | ''      |
    And no side effects

  Scenario: [2] Return a single-quoted string with one character
    Given any graph
    When executing query:
      """
      RETURN 'a' AS literal
      """
    Then the result should be, in any order:
      | literal |
      | 'a'     |
    And no side effects

  Scenario: [3] Return a single-quoted string with uft-8 characters
    Given any graph
    When executing query:
      """
      RETURN '🧐🍌❖⋙⚐' AS literal
      """
    Then the result should be, in any order:
      | literal   |
      | '🧐🍌❖⋙⚐' |
    And no side effects

  Scenario: [6] Return a single-quoted string with 100 characters
    Given any graph
    When executing query:
      """
      RETURN 'zvhg02LrjXbeIWUue4CzFT1baQ5ZA uP0ur4suuufFWZu3MGLlMUDYdhya1WcV8GcpEa4Pi03YjPieg2hJY3rt4OAQIeBKhpasUd' AS literal
      """
    Then the result should be, in any order:
      | literal                                                                                                |
      | 'zvhg02LrjXbeIWUue4CzFT1baQ5ZA uP0ur4suuufFWZu3MGLlMUDYdhya1WcV8GcpEa4Pi03YjPieg2hJY3rt4OAQIeBKhpasUd' |
    And no side effects

  Scenario: [7] Return a single-quoted string with 1000 characters
    Given any graph
    When executing query:
      """
      RETURN '92WeD0wBWj GWB1Y pUd6ZiCalZR5VJzIxXt6C74 4bfhdEAkXIHccJ4Avce2aWXTBj v22FvYQ4F0R GfPsbTyQYaL6DEHMbKR HlnP3BrpNBSO427Tsayra 950dNriiiRPbfLhV5oNHZl1Lbs44oAl40hU4LTkZkzIzNhwDtnOunSXwHH4FWpoqSP7B8VHz88z7X8BoSCECUIVs T4z5UFT9oPUCIsdTjzOocn8nT0dD7PVwRzsO2a4R5sNyYe6R4TdBqIWELcIiKhTpaMQsfuEPuzFnwCV1L g zZhhR7yNIo14oupUUD0V0oIHIRvtM0MITOkSiTTmO68ROtezWPfdJQq9pQ6gdcPsy YAU0wMs dVFBTyTzPml55k VOgY4dEuHUC5BkDGwCm8BTvls07JdY4cwm1zsLq1xGuQfVYmr62WF7VeVVIKFX3FuAIOyFqIshJxA8rTnEtzL1eSxrVcabZ0j24i1Zv2D6SDvsbs45pPHNollnZJmKUkLfrldZzlNEuy4JkJa2ahzizZW72f5m2xiwDKgM3 g7nrbYLgIKUtXOdoJeKgUl2cN7j4Xd30dajZpcIDBqsZ LwmRYQlvRXFafWBMD3yQfU4GEzbWQlxV6iBidK83UVdyyvMKaqPvdqovPVQzhIK Xfs yVwnSHDXpjUonwsOFeykee9TcixuxkbYp3Md EBk4LcBDn4zFR3JSmz3FGfP1llIGL ZYWHrzjugMbxPXU02OrqExStd X1ALxTJq2W6mO4kQig4ZQFKHIs66EVWf6HG3SKAxzPAmmf4DZmlZGawG agiO2PrNnWyifOau4em ozqdkAbxu6mCbMEjMri7dkzpjtYFwkxUGpgSjfDm481Eby3SKvwNybwvqfj5CXHWSjGpk8YtJV0T3jzNd731Wb3SWQrVyIy2Wz1UntzYJ33O W9cFnumIVZK1Sj0pQwWoxktNdyknjXiL5COyZiZDBJOcNtIXoklXdBDy' AS literal
      """
    Then the result should be, in any order:
      | literal                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
      | '92WeD0wBWj GWB1Y pUd6ZiCalZR5VJzIxXt6C74 4bfhdEAkXIHccJ4Avce2aWXTBj v22FvYQ4F0R GfPsbTyQYaL6DEHMbKR HlnP3BrpNBSO427Tsayra 950dNriiiRPbfLhV5oNHZl1Lbs44oAl40hU4LTkZkzIzNhwDtnOunSXwHH4FWpoqSP7B8VHz88z7X8BoSCECUIVs T4z5UFT9oPUCIsdTjzOocn8nT0dD7PVwRzsO2a4R5sNyYe6R4TdBqIWELcIiKhTpaMQsfuEPuzFnwCV1L g zZhhR7yNIo14oupUUD0V0oIHIRvtM0MITOkSiTTmO68ROtezWPfdJQq9pQ6gdcPsy YAU0wMs dVFBTyTzPml55k VOgY4dEuHUC5BkDGwCm8BTvls07JdY4cwm1zsLq1xGuQfVYmr62WF7VeVVIKFX3FuAIOyFqIshJxA8rTnEtzL1eSxrVcabZ0j24i1Zv2D6SDvsbs45pPHNollnZJmKUkLfrldZzlNEuy4JkJa2ahzizZW72f5m2xiwDKgM3 g7nrbYLgIKUtXOdoJeKgUl2cN7j4Xd30dajZpcIDBqsZ LwmRYQlvRXFafWBMD3yQfU4GEzbWQlxV6iBidK83UVdyyvMKaqPvdqovPVQzhIK Xfs yVwnSHDXpjUonwsOFeykee9TcixuxkbYp3Md EBk4LcBDn4zFR3JSmz3FGfP1llIGL ZYWHrzjugMbxPXU02OrqExStd X1ALxTJq2W6mO4kQig4ZQFKHIs66EVWf6HG3SKAxzPAmmf4DZmlZGawG agiO2PrNnWyifOau4em ozqdkAbxu6mCbMEjMri7dkzpjtYFwkxUGpgSjfDm481Eby3SKvwNybwvqfj5CXHWSjGpk8YtJV0T3jzNd731Wb3SWQrVyIy2Wz1UntzYJ33O W9cFnumIVZK1Sj0pQwWoxktNdyknjXiL5COyZiZDBJOcNtIXoklXdBDy' |
    And no side effects

  Scenario: [9] Return a double-quoted empty string
    Given any graph
    When executing query:
      """
      RETURN "" AS literal
      """
    Then the result should be, in any order:
      | literal |
      | ''      |
    And no side effects

  Scenario: [10] Accept valid Unicode literal
    Given any graph
    When executing query:
      """
      RETURN '\u01FF' AS a
      """
    Then the result should be, in any order:
      | a   |
      | 'ǿ' |
    And no side effects

  Scenario: [11] Return a double-quoted string with one character
    Given any graph
    When executing query:
      """
      RETURN "a" AS literal
      """
    Then the result should be, in any order:
      | literal |
      | 'a'     |
    And no side effects

  Scenario: [12] Return a double-quoted string with uft-8 characters
    Given any graph
    When executing query:
      """
      RETURN "🧐🍌❖⋙⚐" AS literal
      """
    Then the result should be, in any order:
      | literal   |
      | '🧐🍌❖⋙⚐' |
    And no side effects
