#!/usr/bin/env bash
# Downloads the openCypher TCK .feature files from GitHub.
# Usage: bash crates/traverse-cypher/tests/download_tck.sh
set -euo pipefail

BASE_URL="https://raw.githubusercontent.com/opencypher/openCypher/master/tck/features"
DEST="$(cd "$(dirname "$0")" && pwd)/features"

echo "Downloading TCK features into $DEST ..."

# directory -> file list
declare -A DIRS
DIRS=(
  # clauses
  ["clauses/call"]="Call1 Call2 Call3 Call4 Call5 Call6"
  ["clauses/create"]="Create1 Create2 Create3 Create4 Create5 Create6"
  ["clauses/delete"]="Delete1 Delete2 Delete3 Delete4 Delete5 Delete6"
  ["clauses/match"]="Match1 Match2 Match3 Match4 Match5 Match6 Match7 Match8 Match9"
  ["clauses/match-where"]="MatchWhere1 MatchWhere2 MatchWhere3 MatchWhere4 MatchWhere5 MatchWhere6"
  ["clauses/merge"]="Merge1 Merge2 Merge3 Merge4 Merge5 Merge6 Merge7 Merge8 Merge9"
  ["clauses/remove"]="Remove1 Remove2 Remove3"
  ["clauses/return"]="Return1 Return2 Return3 Return4 Return5 Return6 Return7 Return8"
  ["clauses/return-orderby"]="ReturnOrderBy1 ReturnOrderBy2 ReturnOrderBy3 ReturnOrderBy4 ReturnOrderBy5 ReturnOrderBy6"
  ["clauses/return-skip-limit"]="ReturnSkipLimit1 ReturnSkipLimit2 ReturnSkipLimit3"
  ["clauses/set"]="Set1 Set2 Set3 Set4 Set5 Set6"
  ["clauses/union"]="Union1 Union2 Union3"
  ["clauses/unwind"]="Unwind1"
  ["clauses/with"]="With1 With2 With3 With4 With5 With6 With7"
  ["clauses/with-orderBy"]="WithOrderBy1 WithOrderBy2 WithOrderBy3 WithOrderBy4"
  ["clauses/with-skip-limit"]="WithSkipLimit1 WithSkipLimit2 WithSkipLimit3"
  ["clauses/with-where"]="WithWhere1 WithWhere2 WithWhere3 WithWhere4 WithWhere5 WithWhere6 WithWhere7"
  # expressions
  ["expressions/aggregation"]="Aggregation1 Aggregation2 Aggregation3 Aggregation4 Aggregation5 Aggregation6 Aggregation7 Aggregation8"
  ["expressions/boolean"]="Boolean1 Boolean2 Boolean3 Boolean4 Boolean5"
  ["expressions/comparison"]="Comparison1 Comparison2 Comparison3 Comparison4"
  ["expressions/conditional"]="Conditional1 Conditional2"
  ["expressions/existentialSubqueries"]="ExistentialSubquery1 ExistentialSubquery2 ExistentialSubquery3"
  ["expressions/graph"]="Graph1 Graph2 Graph3 Graph4 Graph5 Graph6 Graph7 Graph8 Graph9"
  ["expressions/list"]="List1 List2 List3 List4 List5 List6 List7 List8 List9 List10 List11 List12"
  ["expressions/literals"]="Literals1 Literals2 Literals3 Literals4 Literals5 Literals6 Literals7 Literals8"
  ["expressions/map"]="Map1 Map2 Map3"
  ["expressions/mathematical"]="Mathematical1 Mathematical2 Mathematical3 Mathematical4 Mathematical5 Mathematical6 Mathematical7 Mathematical8 Mathematical9 Mathematical10 Mathematical11 Mathematical12 Mathematical13 Mathematical14 Mathematical15 Mathematical16 Mathematical17"
  ["expressions/null"]="Null1 Null2 Null3"
  ["expressions/path"]="Path1 Path2 Path3"
  ["expressions/pattern"]="Pattern1 Pattern2"
  ["expressions/precedence"]="Precedence1 Precedence2 Precedence3 Precedence4"
  ["expressions/quantifier"]="Quantifier1 Quantifier2 Quantifier3 Quantifier4 Quantifier5 Quantifier6 Quantifier7 Quantifier8 Quantifier9 Quantifier10 Quantifier11 Quantifier12"
  ["expressions/string"]="String1 String2 String3 String4 String5 String6 String7 String8 String9 String10 String11 String12 String13 String14"
  ["expressions/temporal"]="Temporal1 Temporal2 Temporal3 Temporal4 Temporal5 Temporal6 Temporal7 Temporal8 Temporal9 Temporal10"
  ["expressions/typeConversion"]="TypeConversion1 TypeConversion2 TypeConversion3 TypeConversion4 TypeConversion5 TypeConversion6"
  # use cases
  ["useCases/countingSubgraphMatches"]="CountingSubgraphMatches1"
  ["useCases/triadicSelection"]="TriadicSelection1"
)

total=0
failed=0

for dir in "${!DIRS[@]}"; do
  mkdir -p "$DEST/$dir"
  for file in ${DIRS[$dir]}; do
    url="$BASE_URL/$dir/$file.feature"
    dest_file="$DEST/$dir/$file.feature"
    if [ -f "$dest_file" ]; then
      total=$((total + 1))
      continue
    fi
    if curl -sSfL -o "$dest_file" "$url"; then
      total=$((total + 1))
    else
      echo "  FAILED: $dir/$file.feature"
      failed=$((failed + 1))
      rm -f "$dest_file"
    fi
  done
done

echo "Done: $total files downloaded, $failed failures."
