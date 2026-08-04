//%includefile option("embedded-test")
//%if option("embassy")
//%include "tests/hello_test_async.rs"
//%else
//%include "tests/hello_test_blocking.rs"
//%endif
