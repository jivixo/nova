# Nova Language Guide

## Variables
```nova
let x = 10
let name = "Alice"
let flag = true
let nothing = nil

x = 20          // reassign — no let needed after first declaration
name = "Bob"    // reassign
```

## Arithmetic
```nova
let a = 10 + 3   // 13         (int)
let b = 10 - 3   // 7          (int)
let c = 10 * 3   // 30         (int)
let d = 10 / 3   // 3.333...   (float — division always produces float)
let e = 10 % 3   // 1          (int)
let f = 1.5 + 2  // 3.5        (float — any float operand promotes the result)
```

## Comparisons
```nova
x == y    // equal
x != y    // not equal
x < y     // less than
x > y     // greater than
x <= y    // less than or equal
x >= y    // greater than or equal

// comparison works on strings too — lexicographic order
"apple" < "banana"    // true
"abc" == "abc"        // true
"z" > "a"             // true
```

## Logical operators
```nova
true && false   // false  (AND)
true || false   // true   (OR)
!true           // false  (NOT)
!false          // true
!!true          // true   (double negation)
```

## Bitwise operators
Integer-only. Precedence: bitwise binds above comparisons, below arithmetic.
```nova
5 & 3     // 1   (AND)
5 | 3     // 7   (OR)
5 ^ 3     // 6   (XOR)
1 << 3    // 8   (left shift)
16 >> 2   // 4   (right shift)

// precedence: arithmetic first, then bitwise, then comparison
2 + 3 & 7         // (2+3) & 7 = 5
val & mask == 0   // (val & mask) == 0

// common patterns
let flags = FLAGS_READ | FLAGS_WRITE   // set bits
flags & FLAGS_READ != 0                // test bit
flags & (255 ^ FLAGS_WRITE)            // clear bit
(pos - 1 + width) & (width - 1)       // ring-buffer wrap (width must be power of 2)
```

## Strings
```nova
let s = "hello"
print s[0]     // "h"   (string indexing — returns a one-char string)
print s[1]     // "e"
print s[-1]    // "o"   (negative index — counts from end)
print s[-2]    // "l"

let greeting = "hello {name}"          // interpolation — bare variable
let info = "avg is {round(avg)}"       // interpolation — function call
let val = "first is {arr[0]}"          // interpolation — index access
let total = "sum is {a + b}"           // interpolation — any expression
let big = upper(s)                     // HELLO
let small = lower("NOVA")              // nova
let trimmed = trim("  hi  ")           // hi
let replaced = replace(s, "l", "r")   // herro
let parts = split("a,b,c", ",")       // [a, b, c]
let joined = join(["x","y"], "-")     // x-y
let sub = substr("hello", 1, 3)       // ell   (start=1, length=3)
let has = contains("hello", "ell")    // true
let sw = startsWith("hello", "hel")  // true
let ew = endsWith("hello", "llo")    // true
let n = len("hello")                  // 5
let n = len([1, 2, 3])               // 3
let n = len({"a": 1, "b": 2})        // 2
```

## Multiline strings
```nova
let poem = """
line one
line two
line three
"""
```

## Null coalescing
```nova
let val = nil
print val ?? "default"   // default

val = "hello"
print val ?? "default"   // hello
```

## If / else if / else
```nova
if x > 10 {
    print "big"
} else if x > 5 {
    print "medium"
} else {
    print "small"
}
```

## While loop
```nova
let i = 0
while i < 5 {
    print i
    i = i + 1   // reassign without let (let is only needed on first declaration)
}
```

## Compound assignment
```nova
let x = 10
x += 5    // x = 15
x -= 3    // x = 12
x *= 2    // x = 24
x /= 4    // x = 6
```

## For loop
```nova
// range
for i in 0..5 {
    print i       // 0 1 2 3 4
}

// array
for item in [10, 20, 30] {
    print item
}

// enumerate — index and value together
for i, item in ["a", "b", "c"] {
    print "{i}: {item}"   // 0: a  1: b  2: c
}

// zip unpacking — unpack pairs directly
let names  = ["Alice", "Bob"]
let scores = [95, 82]
for name, score in zip(names, scores) {
    print "{name}: {score}"   // Alice: 95  Bob: 82
}

// destructure — unpack each element into named variables
for [dx, dy] in [[1,0],[-1,0],[0,1],[0,-1]] {
    print "{dx}, {dy}"
}

// works with any fixed-width pattern
for [x, y, z] in [[1,2,3],[4,5,6]] {
    print x + y + z   // 6, 15
}

// iterating over a string — chars directly
for ch in "hello" {
    print ch   // h e l l o
}
```

The `for [a, b] in arr` form destructures each element. Every element must itself be an array with exactly as many items as there are names. `break` and `continue` work inside destructure loops.

## Break and continue
```nova
for i in 0..10 {
    if i == 3 { continue }   // skip 3
    if i == 6 { break }      // stop at 6
    print i                  // 0 1 2 4 5
}
```

## Functions
```nova
fn add(a, b) {
    a + b           // last expression is returned
}
print add(3, 4)    // 7

// explicit return — exits the function immediately
fn sign(n) {
    if n > 0 { return "positive" }
    if n < 0 { return "negative" }
    return "zero"
}
```

## Default parameters
```nova
fn greet(name, msg = "hello") {
    "{msg}, {name}!"
}
print greet("Alice")           // hello, Alice!
print greet("Bob", "hi")       // hi, Bob!

fn repeat(s, n = 3) {
    let result = ""
    for i in 0..n { result = result + s }
    result
}
print repeat("ab")      // ababab
print repeat("ab", 2)   // abab

// multiple defaults
fn pad(s, width = 10, char = " ") {
    let result = s
    while len(result) < width { result = result + char }
    result
}
print pad("hi", 6, "-")   // hi----
```

Defaults are evaluated once at definition time. Required parameters must come before optional ones (unless all callers always supply all arguments).

## Destructuring
```nova
// array destructuring
let [a, b, c] = [1, 2, 3]
print a    // 1
print b    // 2
print c    // 3

// fewer names than elements — extras ignored
let [x, y] = [10, 20, 30, 40]
print x    // 10
print y    // 20

// more names than elements — extras get nil
let [p, q, r] = [7, 8]
print r    // nil

// hashmap destructuring — names become variables bound to map["name"]
let {name, age} = {"name": "Alice", "age": 30, "city": "NY"}
print name   // Alice
print age    // 30

// missing key → nil
let {score} = {"name": "Bob"}
print score  // nil

// useful in function parameters
fn greet_person(person) {
    let {name, age} = person
    "Hi {name}, you are {age} years old"
}
```

## Calling function results (dynamic calls)
You can call the result of any expression, not just named functions:
```nova
// chained calls — f(1)(2)
fn make_adder(n) { (x) -> x + n }
print make_adder(5)(3)     // 8

// calling a value from an array
let fns = [(x) -> x * 2, (x) -> x * 3]
print fns[0](10)           // 20
print fns[1](10)           // 30

// immediately invoked lambda
print ((x) -> x * x)(7)   // 49

// chained index then call
let ops = {"double": (x) -> x * 2}
print ops["double"](5)     // 10
```

## Type annotations (optional)
Type annotations on function parameters and return types are checked **statically**, before the program runs, not at runtime. Unannotated code works exactly as before.

```nova
fn add(a: int, b: int): int {
    a + b
}

fn greet(name: string): string {
    "hello {name}"
}

// mixing typed and untyped is fine — gradual typing
fn process(data) {
    data   // data is unknown type — no check applied
}
```

Available type names: `int`, `float`, `string`, `bool`, `array`, `hashmap`, `nil`

**Syntax:**
- Parameter type: `param: typename`, e.g. `fn f(x: int)`
- Return type: `): typename` after the closing paren, e.g. `fn f(x: int): string`
- Both are optional. You can annotate some params and not others.

**What gets checked:**
- Passing the wrong type to a typed parameter → compile-time error, program does not start
- A typed function returning the wrong type → compile-time error
- All errors are printed at once before any code runs

**What is allowed:**
- `int` can be passed where `float` is expected (widening, which is mathematically safe)
- Unannotated parameters are treated as unknown and accept anything
- Calling an untyped function never produces a type error

```nova
fn scale(x: float): float {
    x * 2.0
}
scale(5)      // fine — int widens to float automatically
scale("hi")   // type error on line N: argument 1 of 'scale': expected float, got string
```

Multiple errors are reported together:
```nova
fn broken(a: int, b: string): bool {
    return 42
}
broken(true, 99)
// type error: return type mismatch: expected bool, got int
// type error: argument 1 of 'broken': expected int, got bool
// type error: argument 2 of 'broken': expected string, got int
```

## Type inference
Nova infers the return type of unannotated functions from their bodies. Once inferred, that return type is used when checking call sites, even if the function has no annotation.

```nova
// no return annotation — Nova infers return type is int
fn double(n: int) {
    n * 2
}

// double(b) is now known to return int — checked against add's return type
fn add(a: int, b: int): int {
    a + double(b)   // passes — inferred int matches declared int
}
```

Inference also works through chains of unannotated functions, regardless of declaration order:

```nova
fn outer(x: int) {
    inner(x)       // inner defined below — still inferred correctly
}

fn inner(x: int) {
    x + 1          // inferred return: int
}
```

Inference works on explicit returns too:

```nova
fn sign(n: int) {
    if n > 0 { return "positive" }
    if n < 0 { return "negative" }
    return "zero"
}
// all return paths return string → inferred return type: string
```

If return paths disagree, the return type stays unknown (no error, no check):

```nova
fn ambiguous(n: int) {
    if n > 0 { return "positive" }
    return 0    // string vs int — can't infer, stays unknown
}
```

**What inference covers:**
- Literal values (`42` → int, `"hi"` → string, `true` → bool, `3.14` → float)
- Arithmetic (`int + int` → int, `int / int` → float, any float operand → float)
- Comparisons and logical operators → bool
- Calls to other typed or already-inferred functions → their return type
- Variables that are typed parameters → their declared type

**What stays unknown (no error):**
- Variables with no annotation that aren't typed params
- Calls to functions whose return type can't be determined
- Any expression the checker can't fully resolve

## Generics

Generic functions work on any element type. Nova resolves the type variable `T` from the actual arguments at each call site and uses it to type-check the return value.

```nova
fn first<T>(arr: [T]) -> T {
    return arr[0]
}

print first([10, 20, 30])     // 10  — T resolved to int,    return type: int
print first(["hi", "bye"])    // hi  — T resolved to string, return type: string
```

**Syntax:**
- Type parameters: `<T>` or `<T, U>` after the function name
- Typed array annotation: `arr: [T]`, an array whose elements are type `T`
- Return type uses `->` or `:`, both are valid:
  - `fn first<T>(arr: [T]) -> T`: arrow style (familiar from other languages)
  - `fn first<T>(arr: [T]): T`: colon style (Nova's original syntax)

**Multiple type variables:**
```nova
fn identity<T>(x: T) -> T {
    return x
}

print identity(42)       // 42   (int)
print identity("nova")   // nova (string)
```

**How it works at check time:**
1. Nova looks at the actual argument types at the call site
2. It unifies them against the declared parameter types to figure out what `T` is, e.g. `first([1,2,3])` passes `[int]` for `[T]`, so `T = int`
3. It substitutes `T = int` into the return type annotation. The call is now known to return `int`
4. If that return type is used somewhere with an incompatible type, it becomes a compile-time error

**Generics are erased at runtime.** No boxing, no overhead, no reflection. They exist only during type checking and have zero cost in the evaluator.

**Array element types flow through automatically:**
```nova
let nums = [10, 20, 30]      // inferred as [int]
let s = first(nums)          // return type inferred as int
```

The type checker tracks `[1, 2, 3]` as `[int]` (not just `array`), so element types propagate through generic function calls, index expressions, and for loops:
```nova
fn echo_each<T>(items: [T]) {
    for item in items {
        print item   // item has type T (resolved from the call site)
    }
}
```

**What stays unknown:**
Generics are gradual. If `T` can't be resolved (e.g. the array comes from an untyped source), the return type stays Unknown and no error is produced. Nova only errors when it has enough information to be certain.

## Structs

```nova
// define a struct — name must start with uppercase
struct Point { x, y }
struct Person { name, age }

// instantiate
let p = Point { x: 3, y: 4 }
let alice = Person { name: "Alice", age: 30 }

// field access
print p.x        // 3
print alice.name // Alice

// field mutation
p.x = 10
alice.age = 31

// nested structs
struct Rect { origin, size }
struct Size { w, h }
let r = Rect { origin: Point { x: 0, y: 0 }, size: Size { w: 100, h: 50 } }
print r.size.w      // 100
r.size.w = 200      // nested mutation

// structs are passed by reference — mutation inside a function is visible outside
fn move_point(pt, dx, dy) {
    pt.x = pt.x + dx
    pt.y = pt.y + dy
}
move_point(p, 5, 3)
print p.x   // 15

// struct returned from a function
fn make_point(a, b) {
    Point { x: a, y: b }
}
let q = make_point(7, 8)

// type() on a struct
print type(p)   // struct:Point
```

> **Note:** struct field names are not validated against the definition at runtime. Extra fields are silently accepted. Field type checking is planned for a future phase.

> **Reference semantics:** structs are reference types. `let p2 = p` makes both variables point to the same struct. Mutating `p2.x` also changes `p.x`. To get an independent copy, you need to construct a new struct manually.

## Methods

Use `impl` to attach methods to a struct type. The first parameter is always `self`, the instance the method was called on.

```nova
struct Point { x, y }

impl Point {
    fn distance(self) {
        sqrt(self.x * self.x + self.y * self.y)
    }

    fn translate(self, dx, dy) {
        self.x += dx
        self.y += dy
    }

    fn to_str(self) {
        "(" + str(self.x) + ", " + str(self.y) + ")"
    }
}

let p = Point { x: 3, y: 4 }
print p.distance()       // 5.0
p.translate(1, 2)
print p.to_str()         // (4, 6)
```

- Methods can read and mutate `self` fields directly
- Mutation inside a method is visible after the call (structs use reference semantics)
- Method results can be used in any expression: `let d = p.distance()`
- `impl` blocks are separate from `struct` declarations. Multiple `impl` blocks for the same type are fine
- Methods are only supported on structs; calling a method on any other type is a runtime error

> **Compound field assignment** works inside and outside methods: `p.x += 5`, `p.y *= 2`, etc.

## Enums

```nova
// unit variants — no payload
enum Direction { North, South, East, West }

let d = Direction.North
print d           // North
print type(d)     // enum:Direction:North

// payload variants — carry data
enum Shape {
    Circle(radius)
    Rect(width, height)
    Point
}

let c = Shape.Circle(5)
let r = Shape.Rect(10, 20)
let pt = Shape.Point

// match on enum variants with payload destructuring
match c {
    Shape.Circle(r)     => print "circle with radius {r}"
    Shape.Rect(w, h)    => print "rect {w}x{h}"
    Shape.Point         => print "just a point"
    _                   => print "unknown"
}

// match as an expression — return value assigned
fn describe(shape) {
    match shape {
        Shape.Circle(r)     => "circle r={r}"
        Shape.Rect(w, h)    => "rect {w}x{h}"
        Shape.Point         => "point"
        _                   => "unknown"
    }
}
print describe(Shape.Circle(3))    // circle r=3
print describe(Shape.Rect(4, 5))   // rect 4x5
print describe(Shape.Point)        // point

// constructors as first-class values — store and call later
let make_circle = Shape.Circle
let c2 = make_circle(7)   // same as Shape.Circle(7)

// enums in arrays
let shapes = [Shape.Circle(1), Shape.Rect(2, 3), Shape.Point]
for s in shapes {
    print describe(s)
}

// equality — same variant + same payload = equal
let a = Shape.Circle(5)
let b = Shape.Circle(5)
print a == b                   // true
print a == Shape.Circle(6)     // false
print a == Shape.Rect(5, 1)    // false

// type()
print type(Direction.North)    // enum:Direction:North
print type(Shape.Circle(5))    // enum:Shape:Circle
print type(Shape)              // enum_def:Shape

// enums inside structs
struct Event { kind, data }
let ev = Event { kind: Shape.Circle(10), data: "click" }
match ev.kind {
    Shape.Circle(r) => print "event radius {r}"
    _               => print "other event"
}
```

## if-else as an expression
```nova
// if-else can appear anywhere an expression is expected
let sign = if x > 0 { 1 } else { -1 }

fn classify(n) {
    let label = if n < 0 { "neg" } else if n == 0 { "zero" } else { "pos" }
    label
}

// works inside other expressions
let masked = (if x > 10 { 255 } else { 0 }) & x
```
An `if` without an `else` is a statement (returns nil). An `if`-`else` chain is an expression.

## Recursion
```nova
fn fib(n) {
    if n <= 1 { n } else { fib(n-1) + fib(n-2) }
}
print fib(10)   // 55
```

## Variadic functions
```nova
fn log(items...) {
    for item in items {
        print item
    }
}
log("a", "b", "c")   // prints each on its own line

// regular params before the variadic
fn labeled(label, items...) {
    printn "{label}: "
    for item in items { printn "{item} " }
    print ""
}
labeled("nums", 1, 2, 3)   // nums: 1 2 3
```

## Lambdas
```nova
let double = (x) -> x * 2
let add    = (a, b) -> a + b

print double(5)     // 10
print add(3, 4)     // 7
```

## Closures
```nova
fn make_adder(n) {
    (x) -> x + n    // captures n from the enclosing scope
}
let add5 = make_adder(5)
print add5(10)   // 15
```

## Pipe operator
```nova
let result = [3, 1, 2] |> sort() |> len()
print result   // 3

"  hello  " |> trim() |> upper() |> print   // HELLO
```

## Pattern matching
```nova
// exact values
match score {
    100 => print "perfect"
    90  => print "great"
    _   => print "ok"       // _ is the wildcard
}

// range patterns (exclusive on the right, same as for loops)
match score {
    90..101 => print "A"
    70..90  => print "B"
    50..70  => print "C"
    _       => print "F"
}

// works with strings too
match name {
    "Alice" => print "hi Alice"
    "Bob"   => print "hi Bob"
    _       => print "hi stranger"
}

// nil pattern
match some_value {
    nil => print "no value"
    _   => print "has value"
}

// booleans
match flag {
    true  => print "yes"
    false => print "no"
}

// match as an expression — result assigned to a variable
let label = match score {
    90..101 => "A"
    70..90  => "B"
    _       => "F"
}

// commas between arms are optional — useful for inline match
let label = match score { 90..101 => "A", 70..90 => "B", _ => "F" }

// nested match
fn classify(n) {
    match n > 0 {
        true => match n > 100 {
            true  => "big"
            false => "small"
        }
        false => "non-positive"
    }
}

// match on enum variants — see Enums section for full examples
enum Result { Ok(value), Err(message) }

match Result.Ok(42) {
    Result.Ok(v)  => print "success: {v}"
    Result.Err(e) => print "error: {e}"
}

// wildcard still works alongside enum patterns
match some_result {
    Result.Ok(v) => print v
    _            => print "failed"
}
```

## Arrays
```nova
let arr = [1, 2, 3]
print arr[0]              // 1
print arr[-1]             // 3   (negative index — counts from end)
print arr[-2]             // 2
arr[0] = 99               // mutation
arr[-1] = 0               // mutation via negative index

// chained indexing — works on nested arrays and hashmaps
let nested = [[1, 2], [3, 4]]
print nested[0][1]        // 2
print nested[1][0]        // 3
print len(arr)            // 3
let arr = push(arr, 4)    // [99, 2, 3, 4]
let arr = pop(arr)        // [99, 2, 3]
let arr = reverse(arr)    // [3, 2, 99]
let arr = sort(arr)       // [2, 3, 99]
let s = slice(arr, 0, 2)  // [2, 3]
let c = concat(arr, [4, 5])          // join two arrays: [2, 3, 99, 4, 5]
let z = zip([1,2,3], ["a","b","c"])  // [[1,a],[2,b],[3,c]]
```

### Array copy semantics
Assigning an array to a new variable gives you an independent copy. Mutating one does not affect the other.
```nova
let a = [1, 2, 3]
let b = a
b[0] = 99
print a[0]   // 1  — a is unchanged
print b[0]   // 99 — b has its own copy
```
This also applies when passing arrays to functions. The function gets its own copy and cannot modify the caller's array.

### Pre-sized arrays
`make_array(n, default)` creates an array of exactly `n` elements all set to `default`. Faster than building with a loop when the size is known up front and you intend to fill by index rather than push.

```nova
let arr = make_array(10, 0)     // [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
arr[3] = 42                     // set element by index
let grid = make_array(512 * 512, 0)   // flat 512x512 grid, all zeros
```

## Higher-order functions
```nova
let nums = [1, 2, 3, 4, 5]
print map(nums, (x) -> x * 2)              // [2, 4, 6, 8, 10]
print filter(nums, (x) -> x > 2)           // [3, 4, 5]
print reduce(nums, 1, (acc, x) -> acc * x) // 120  (multiply all — product)
print find(nums, (x) -> x > 3)             // 4
print contains(nums, 3)                    // true
print sum(nums)                            // 15
print product(nums)                        // 120
print any(nums, (x) -> x > 4)             // true
print all(nums, (x) -> x > 0)             // true
print count(nums, (x) -> x % 2 == 0)      // 2
```

## Hash maps
```nova
let m = {"name": "Alice", "age": 30}
print m["name"]             // Alice
print hasKey(m, "name")     // true
print keys(m)               // [name, age]
print values(m)             // [Alice, 30]
print m["missing"]          // nil  (missing key returns nil, not an error)
print m["missing"] ?? 0     // 0    (use ?? to provide a default)

// direct key assignment — creates or overwrites
m["age"] = 31
m["city"] = "NY"
let k = "role"
m[k] = "admin"              // dynamic key from variable

// compound assignment on map values
m["age"] = m["age"] + 1     // read, modify, write back

// nested map mutation requires an intermediate variable (CoW)
let db = config["db"]
db["port"] = 5433
config["db"] = db

let m = mergeMap(m, {"city": "NY"})   // merge two maps
let m = setKey(m, "role", "admin")    // set a key (functional style)
let m = delete(m, "age")              // remove a key
```

## Timing
`clock()` returns the current wall-clock time as an integer number of milliseconds since the Unix epoch. Subtract two readings to measure elapsed time.

```nova
let t0 = clock()
// ... work ...
let t1 = clock()
println("elapsed: " + str(t1 - t0) + " ms")
```

## Math built-ins
```nova
sqrt(144)          // 12.0    (always float)
abs(-5)            // 5       (same type as input — int in, int out)
abs(-5.5)          // 5.5     (float in, float out)
floor(3.9)         // 3       (always int)
ceil(3.1)          // 4       (always int)
round(3.5)         // 4       (no decimals arg → int)
round(3.14159, 2)  // 3.14    (with decimals arg → float)
pow(2, 10)         // 1024.0  (always float)
max(3, 7)          // 7       (two ints → int)
max([3, 1, 4])     // 4       (array)
min(3, 7)          // 3       (two ints → int)
min([3, 1, 4])     // 1       (array)
random()           // random float between 0 and 1
```

## Type checking
```nova
type(42)          // int
type(3.14)        // float
type("hi")        // string
type(true)        // bool
type([1,2])       // array
ord("a")          // 97   (Unicode code point of a character)
ord("Z")          // 90
chr(97)           // "a"  (character from a code point)
chr(65)           // "A"
isInt(42)         // true
isFloat(3.14)     // true
isString("hi")    // true
isBool(true)      // true
isArray([])       // true
isHashmap({})     // true
isNil(nil)        // true
str(42)           // "42"
str(3.14)         // "3.14"
str(true)         // "true"
str(nil)          // "nil"
int("42")         // 42
int(3.9)          // 3    (truncates toward zero)
int(true)         // 1
int(false)        // 0
int("abc")        // Error — throws instead of silently returning 0
float(42)         // 42.0
float("3.14")     // 3.14
float(true)       // 1.0
float(false)      // 0.0
float("abc")      // Error — throws
mod(-1, 26)       // 25  (true mathematical modulo — always non-negative)
mod(7, 3)         // 1
mod(-7, 3)        // 2
// % is remainder (can be negative): -1 % 26 == -1
// mod() is modulo (always non-negative): mod(-1, 26) == 25
```

## IO
```nova
print "hello"                        // print with newline
printn "hello "                      // print WITHOUT newline (stays on same line)
println("hello")                     // print with newline (function form)
let name = input("What's your name? ")
let text = readFile("file.txt")
writeFile("out.txt", "hello nova")
```

## Error handling
```nova
// throw any value — string, number, hashmap, anything
try {
    throw "something went wrong"
} catch err {
    print "caught: {err}"   // caught: something went wrong
}

// throw a hashmap for structured errors
try {
    throw {"code": 404, "message": "not found"}
} catch err {
    print err["code"]      // 404
    print err["message"]   // not found
}

// throw inside a function
fn divide(a, b) {
    if b == 0 { throw "division by zero" }
    a / b
}

try {
    let result = divide(10, 0)
} catch err {
    print "error: {err}"   // error: division by zero
}

// runtime errors are also catchable
try {
    int("not a number")
} catch err {
    print err   // Error on line N: int() cannot convert "not a number" to int
}

// catch variable is scoped to the catch block — does not leak
let err = "original"
try {
    throw "new value"
} catch err {
    print err   // new value
}
print err       // original — outer err is unchanged

// nested try blocks — inner throw can be rethrown
try {
    try {
        throw "inner"
    } catch e {
        throw "rethrown: {e}"
    }
} catch e {
    print e   // rethrown: inner
}

// throw is an expression — valid inside lambdas, ?? fallbacks, match arms
let safe = map["key"] ?? throw "missing key"
let proc = processor_map[name] ?? ((_) -> throw "unknown processor")
let val = match x { Some(v) => v, _ => throw "expected Some" }
```

## Modules
```nova
import "utils.nova"   // runs utils.nova and brings all its functions/variables into scope

greet("Alice")   // function defined in utils.nova
print PI         // variable defined in utils.nova
```

> **Note:** `import` currently dumps everything into the global scope. Name collisions are possible.
> Circular imports are detected and produce a runtime error.
> Namespaced imports (`import "x" as x`) are planned for a future phase.

## Semicolons

Nova does **not require** semicolons. Almost all Nova code works fine without them.

**When you need one:** when a line ends with a callable expression AND the next line starts with `(`. The parser is greedy. It reads `expr \n (...)` as a function call `expr(...)`, not two separate statements.

```nova
// BREAKS — parser reads {}(x) as a call on the hashmap, then -> is unexpected
let cache = {}
(x) -> x + 1

// FIXED — semicolon stops the greedy parsing
let cache = {};
(x) -> x + 1
```

The same issue can occur after any expression that ends with `]`, `)`, or `}` if the next line opens with `(`:
```nova
let arr = [1, 2, 3]
(x) -> x + 1       // parsed as arr[...](x) chain — add ; after the array

let result = foo()
(x) -> x + 1       // parsed as foo()(x) — add ; after the call
```

**When you don't need one:** everywhere else: `let`, `if`, `while`, `for`, `fn`, plain expressions, anything where the next line does NOT start with `(`. That covers the vast majority of Nova code.

```nova
// all fine without semicolons
let x = 10
let y = x + 1
if x > 5 { print "big" }
for i in 0..3 { print i }
fn add(a, b) { a + b }
```

## Comments
```nova
// this is a comment
let x = 5   // inline comment
```

## REPL
```
nova                                    // launch interactive REPL (bytecode VM)
nova run file.nova                      // run a file (LLVM default, VM fallback)
nova run --vm file.nova                 // force the bytecode VM
nova run --tree file.nova               // force the tree-walker
nova run --tree --memory file.nova      // run with memory report (tree-walker only)
quit                                    // exit the REPL
```

The REPL runs on the bytecode VM. Global mutation works correctly, there is no recursion depth limit, and functions defined in one line are available in subsequent lines. Expression results are auto-printed without needing `print`.

The `--memory` flag (tree-walker only) prints a report to stderr after the program finishes:
```
--- memory report ---
arrays allocated:    50
hashmaps allocated:  12
cycles collected:    0
live arrays:         11
live hashmaps:       4
peak live objects:   15
```
- **arrays/hashmaps allocated**: total objects created during the run
- **cycles collected**: objects freed by the cycle detector (usually 0, CoW prevents most cycles)
- **live**: objects still in scope at program end
- **peak live objects**: highest number of objects alive at any point during execution

## Channels

Channels are first-class values for message passing. Values sent through a channel are always deep-cloned. Each receiver owns fully independent data.

```nova
let ch = channel()      // create an unbounded channel

send(ch, 42)            // send a value (deep-cloned on send)
let v = recv(ch)        // blocking receive — returns nil if channel is closed
print v                 // 42

send(ch, "hello")
let t = tryRecv(ch)     // non-blocking — returns nil if queue is empty
print t                 // hello

close(ch)               // signal that no more values will be sent
```

Channels are reference types. Assigning or passing a channel shares the same queue:

```nova
let ch = channel()
let alias = ch
send(alias, 99)
print recv(ch)          // 99 — same underlying channel
```

Channels can carry any value including arrays, structs, and enums. The sender's copy and the receiver's copy are always independent:

```nova
let ch = channel()
let arr = [1, 2, 3]
send(ch, arr)
push(arr, 99)           // mutate original after send
let got = recv(ch)
print got               // [1, 2, 3] — unaffected by the mutation
```

Channels are first-class. Store them in arrays, pass to functions, return from functions:

```nova
let ch = channel()
let t = type(ch)        // "channel"
let s = str(ch)         // "<channel>"
```

---

## spawn + wait

`spawn` starts an expression on a new OS thread and returns a task handle. `wait` blocks until the task finishes and returns its result. On the LLVM backend (default), each `spawn` creates a real OS thread. On the VM backend, `spawn` uses a work-stealing thread pool.

```nova
fn double(n) { n * 2 }

let t = spawn double(21)   // starts immediately on a new thread
let result = wait(t)       // blocks until done
print result               // 42
```

Multiple tasks run in parallel:

```nova
fn square(n) { n * n }

let t1 = spawn square(10)
let t2 = spawn square(20)
let t3 = spawn square(30)

print wait(t1)   // 100
print wait(t2)   // 400
print wait(t3)   // 900
```

Tasks are share-nothing. Args are deep-cloned on spawn so mutations inside the task never affect the parent:

```nova
fn add_element(arr) {
    len(push(arr, 99))   // sees a copy — original is untouched
}

let original = [1, 2, 3]
let t = spawn add_element(original)
print wait(t)            // 4
print len(original)      // 3 — unchanged
```

If a spawned task throws, the error is re-thrown in the parent when `wait` is called:

```nova
fn risky() { throw "something went wrong" }

try {
    let t = spawn risky()
    wait(t)              // re-throws here
} catch err {
    print err            // spawned task threw: something went wrong
}
```

Task handles are first-class values:

```nova
let t = spawn double(5)
print type(t)            // "task"
print str(t)             // "<task>"
```

---

## spawnAll

`spawnAll(array, fn)` runs `fn(element)` for every element concurrently, waits for all tasks to finish, and returns the results in the **original order**.

```nova
fn fib(n) {
    if n <= 1 { n } else { fib(n - 1) + fib(n - 2) }
}

let results = spawnAll([5, 6, 7, 8], fib)
print results   // [5, 8, 13, 21]
```

`spawnAll` is structured concurrency. It spawns and joins all tasks for you. No manual channel setup, no handle tracking.

Equivalent to:
```nova
let t1 = spawn fib(5)
let t2 = spawn fib(6)
let t3 = spawn fib(7)
let t4 = spawn fib(8)
let results = [wait(t1), wait(t2), wait(t3), wait(t4)]
```

Works with lambdas:
```nova
let scaled = spawnAll([1, 2, 3, 4, 5], (n) -> n * 10)
print scaled   // [10, 20, 30, 40, 50]
```

Works with closures that capture outer variables:
```nova
let factor = 7
let results = spawnAll([1, 2, 3], (n) -> n * factor)
print results   // [7, 14, 21]
```

Results are always in the same order as the input array, regardless of which threads finish first.

---

## select

`select` blocks until one of several channels has a message, then runs the matching arm and binds the received value to the given variable.

```nova
select {
    case ch1 -> v {
        print "got from ch1: {v}"
    }
    case ch2 -> v {
        print "got from ch2: {v}"
    }
}
```

Whichever channel is ready first wins. The others are untouched.

```nova
let ch1 = channel()
let ch2 = channel()
spawn send(ch2, "hello")

select {
    case ch1 -> v { print "from ch1" }  // ch1 has nothing — skipped
    case ch2 -> v { print "from ch2: {v}" }  // prints: from ch2: hello
}
```

`select` can be used in a loop to process messages from multiple channels as they arrive:

```nova
while true {
    select {
        case work_ch -> job  { process(job) }
        case done_ch -> stop { break }
    }
}
```

## select default (non-blocking)

Add a `default` arm to make `select` non-blocking. If no channel has a sender waiting right now, `default` runs immediately instead of blocking.

```nova
select {
    case ch -> v { print "got: {v}" }
    default { print "nothing ready" }
}
```

Typical use: poll loop. Keep doing work and check a channel periodically:

```nova
let done = false
while !done {
    select {
        case result_ch -> v {
            print "result: {v}"
            let done = true
        }
        default {
            // do other work here, then loop back
        }
    }
}
```

`default` must be the last arm and can only appear once.

---

## defer

`defer expr` schedules `expr` to run when the enclosing function returns, regardless of whether the return is normal or early. Multiple defers run in **LIFO order** (last registered, first executed).

```nova
fn process(f) {
    defer close(f)       // runs when process() returns, no matter what
    // ... use f ...
}
```

Multiple defers:

```nova
fn setup() {
    defer send(log, "teardown-3")   // runs 3rd (first registered)
    defer send(log, "teardown-2")   // runs 2nd
    defer send(log, "teardown-1")   // runs 1st (last registered)
    // ... function body ...
}
```

Defer runs even on early return:

```nova
fn read_file(path) {
    let f = open(path)
    defer close(f)          // guaranteed to run
    if !f { return nil }    // early return — defer still fires
    return parse(f)
}
```

`defer` only has effect inside a function. At the top level it is a no-op.

---

## ticker

`ticker(ms)` returns a channel that receives `true` every N milliseconds. Use it with `select` to drive periodic work.

```nova
let tick = ticker(100)    // fires every 100ms
let count = 0
while count < 5 {
    select {
        case tick -> _ { let count = count + 1 }
    }
}
print count    // 5
```

The background thread stops automatically when the channel is no longer referenced.

---

## timeout

`timeout(ms)` returns a channel that receives `true` once after N milliseconds.

```nova
let t = timeout(5000)    // fires once after 5 seconds

select {
    case result_ch -> r { print "got result: {r}" }
    case t -> _          { print "timed out" }
}
```

Combined with `select default`, you can check whether the timeout has fired yet without blocking:

```nova
let t = timeout(200)
select {
    case t -> _ { print "already fired" }
    default      { print "not yet" }
}
```

---

## mutex

A mutex protects a shared value so only one task can read or modify it at a time.

```nova
let m = mutex(0)       // create a mutex holding initial value 0
```

**`lock(m)`**: takes the value out of the mutex (blocks until it's available):

```nova
let val = lock(m)
// ... work with val ...
unlock(m, val)         // put a (possibly new) value back
```

**`unlock(m, new_val)`**: releases the mutex with a new value. Must always be called after every `lock`.

**`withLock(m, fn)`**: the safe pattern. Atomically locks, calls `fn(current)`, stores the result, returns it. No risk of forgetting to unlock.

```nova
let counter = mutex(0)

withLock(counter, (n) -> n + 1)   // 0 → 1
withLock(counter, (n) -> n + 1)   // 1 → 2
withLock(counter, (n) -> n + 1)   // 2 → 3

let v = lock(counter)
print(v)       // 3
unlock(counter, v)
```

`withLock` also returns the new value:

```nova
let bank = mutex(100)
let after = withLock(bank, (bal) -> bal - 30)
print(after)   // 70
```

**Concurrent safe increment:** the classic use case:

```nova
let shared = mutex(0)

fn worker(m) {
    let i = 0
    while i < 1000 {
        withLock(m, (n) -> n + 1)
        let i = i + 1
    }
}

let t1 = spawn worker(shared)
let t2 = spawn worker(shared)
wait(t1)
wait(t2)

let result = lock(shared)
print(result)   // always exactly 2000
unlock(shared, result)
```

`type(m)` returns `"mutex"`.

---
