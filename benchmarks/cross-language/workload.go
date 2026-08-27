package main

import (
	"fmt"
	"os"
)

func checksum() uint64 {
	const prime uint64 = 1000000007
	var numeric uint64 = 17
	var graph uint64 = 29
	var text uint64 = 7
	for index := uint64(0); index < 500000; index++ {
		numeric = (numeric*1664525 + index*1013904223 + 12345) % prime
		graph = (graph + ((index*31+17)%9973)*((index%13)+1)) % prime
		lanes := [...]uint64{84, 78, 67, 72}
		text = (text + lanes[index%4]) % prime
	}
	return (numeric + graph*31 + text*131) % prime
}

func main() {
	if checksum() != 899120682 {
		fmt.Println("checksum=invalid")
		os.Exit(1)
	}
	fmt.Println("checksum=899120682")
}
