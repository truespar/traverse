using Xunit.Sdk;
using Xunit.v3;

// The server runs one database load/unload/drop at a time and answers a second with 409, and every
// class's fixture creates and drops a database of its own: the classes take turns.
[assembly: Parallelization(Mode = ParallelMode.None)]
