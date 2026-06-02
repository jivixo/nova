# Nova: Code Examples

A set of focused, runnable examples covering the core language features.

---

## 1. Hello World

```nova
print "hello, world"
```

```
hello, world
```

---

## 2. Fibonacci

Recursive definition with implicit return (the last expression is the return value).

```nova
fn fib(n) {
    if n <= 1 { n } else { fib(n - 1) + fib(n - 2) }
}

for i in 0..10 {
    print "{i}: {fib(i)}"
}
```

```
0: 0
1: 1
2: 1
3: 2
4: 3
5: 5
6: 8
7: 13
8: 21
9: 34
```

For large inputs use `nova run` (LLVM backend) or `nova run --vm` to avoid the tree-walker's 2000-frame recursion cap.

---

## 3. Closures and Higher-Order Functions

Closures capture the enclosing scope at creation time. Named functions capture globals but not enclosing locals, so use lambdas for true closures.

```nova
fn make_adder(n) {
    (x) -> x + n    // lambda captures n
}

let add5  = make_adder(5)
let add10 = make_adder(10)

print add5(3)    // 8
print add10(3)   // 13

// higher-order pipeline
let nums = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]

let result = nums
    |> filter((x) -> x % 2 == 0)
    |> map((x) -> x * x)
    |> sum()

print result    // 220  (4 + 16 + 36 + 64 + 100)
```

```
8
13
220
```

---

## 4. Structs with Methods

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
        "({self.x}, {self.y})"
    }
}

let p = Point { x: 3.0, y: 4.0 }
print p.distance()       // 5.0
p.translate(1.0, -1.0)
print p.to_str()         // (4.0, 3.0)

// structs use reference semantics — mutation is visible after the call
fn mirror(pt) {
    let tmp = pt.x
    pt.x = pt.y
    pt.y = tmp
}

let q = Point { x: 1.0, y: 2.0 }
mirror(q)
print q.to_str()    // (2.0, 1.0)
```

```
5.0
(4.0, 3.0)
(2.0, 1.0)
```

---

## 5. Enums and Pattern Matching

```nova
enum Shape {
    Circle(radius)
    Rect(width, height)
    Point
}

fn area(s) {
    match s {
        Shape.Circle(r)     => 3.14159 * r * r
        Shape.Rect(w, h)    => w * h
        Shape.Point         => 0
    }
}

fn describe(s) {
    match s {
        Shape.Circle(r)     => "circle with radius {r}"
        Shape.Rect(w, h)    => "rectangle {w}x{h}"
        Shape.Point         => "a point"
        _                   => "unknown shape"
    }
}

let shapes = [Shape.Circle(5), Shape.Rect(3, 4), Shape.Point]
for s in shapes {
    print "{describe(s)} — area = {area(s)}"
}
```

```
circle with radius 5 — area = 78.53975
rectangle 3x4 — area = 12
a point — area = 0
```

---

## 6. Concurrent Spawn and Wait

`spawn` launches an expression on a new OS thread. `wait` blocks until the task finishes and returns its value. Arguments are deep-cloned on spawn so there is no shared state and no data races.

```nova
fn fib(n) {
    if n <= 1 { n } else { fib(n - 1) + fib(n - 2) }
}

// run four fibonacci computations in parallel
let t1 = spawn fib(30)
let t2 = spawn fib(28)
let t3 = spawn fib(25)
let t4 = spawn fib(20)

print wait(t1)    // 832040
print wait(t2)    // 317811
print wait(t3)    // 75025
print wait(t4)    // 6765

// spawnAll: fan-out over an array, results in input order
let results = spawnAll([30, 28, 25, 20], fib)
print results     // [832040, 317811, 75025, 6765]
```

```
832040
317811
75025
6765
[832040, 317811, 75025, 6765]
```

---

## 7. Channels

Channels are first-class, typed message queues. Values sent through a channel are deep-cloned so the sender and receiver always own independent data.

```nova
fn producer(ch, n) {
    for i in 0..n {
        send(ch, i * i)
    }
    close(ch)
}

fn consumer(ch, n) {
    let total = 0
    for _ in 0..n {
        let v = recv(ch)
        total = total + v
    }
    total
}

let ch = channel()
let n  = 5

let sender   = spawn producer(ch, n)
let receiver = spawn consumer(ch, n)

wait(sender)
let sum = wait(receiver)
print "sum of squares 0..{n}: {sum}"    // 0+1+4+9+16 = 30
```

```
sum of squares 0..5: 30
```

---

## 8. Error Handling: try / catch / throw

`throw` accepts any value. Runtime errors (bad index, type mismatch, division by zero) are also catchable.

```nova
fn safe_div(a, b) {
    if b == 0 { throw {"code": 400, "msg": "division by zero"} }
    a / b
}

fn parse_positive(s) {
    let n = int(s)
    if n <= 0 { throw "expected a positive number, got {s}" }
    n
}

// structured error (hashmap)
try {
    print safe_div(10, 0)
} catch err {
    print "error {err["code"]}: {err["msg"]}"
}

// string error
try {
    print parse_positive("-5")
} catch err {
    print "caught: {err}"
}

// runtime error (bad conversion)
try {
    let n = int("not a number")
    print n
} catch err {
    print "runtime: {err}"
}

// rethrow
try {
    try {
        throw "inner problem"
    } catch e {
        throw "wrapped: {e}"
    }
} catch e {
    print e
}
```

```
error 400: division by zero
caught: expected a positive number, got -5
runtime: Error on line N: int() cannot convert "not a number" to int
wrapped: inner problem
```

---

## 9. FizzBuzz

```nova
for i in 1..21 {
    if i % 15 == 0 {
        print "FizzBuzz"
    } else if i % 3 == 0 {
        print "Fizz"
    } else if i % 5 == 0 {
        print "Buzz"
    } else {
        print i
    }
}
```

```
1
2
Fizz
4
Buzz
Fizz
7
8
Fizz
Buzz
11
Fizz
13
14
FizzBuzz
...
```

---

## 10. Triangle (printn)

`printn` prints without a newline. Combined with `print ""` at the end of each row it builds a triangle.

```nova
for i in 0..5 {
    for j in 0..i {
        printn "* "
    }
    print ""
}
```

```

*
* *
* * *
* * * *
```

---

## 11. Grade Calculator

Pattern matching on ranges maps a numeric score to a letter grade.

```nova
fn grade(score) {
    match score {
        90..101 => "A"
        70..90  => "B"
        50..70  => "C"
        _       => "F"
    }
}

let scores = [92, 85, 67, 45, 73]
for s in scores {
    print "{s} -> {grade(s)}"
}
```

```
92 -> A
85 -> B
67 -> C
45 -> F
73 -> B
```

---

## 12. Higher-Order Pipeline

`filter` and `map` chained with the pipe operator `|>`.

```nova
let words = ["banana", "apple", "cherry", "avocado"]
let result = words
    |> filter((w) -> startsWith(w, "a"))
    |> map((w) -> upper(w))
print result
```

```
[APPLE, AVOCADO]
```

---

## 13. Caesar Cipher

Iterates over a string character by character, shifts only lowercase letters using `ord`/`chr` and the true mathematical modulo (`mod`) so negative shifts wrap correctly.

```nova
fn caesar(text, shift) {
    let result = ""
    for ch in text {
        let c = ord(ch)
        if c >= 97 && c <= 122 {
            result = result + chr(mod(c - 97 + shift, 26) + 97)
        } else {
            result = result + ch
        }
    }
    result
}

print caesar("hello", 3)      // khoor
print caesar("khoor", -3)     // hello
print caesar("hello", 13)     // uryyb  (ROT13)
print caesar("uryyb", 13)     // hello  (ROT13 is its own inverse)
```

```
khoor
hello
uryyb
hello
```
