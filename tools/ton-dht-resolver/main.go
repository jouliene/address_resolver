package main

import (
	"context"
	"crypto/ed25519"
	"crypto/rand"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net"
	"os"
	"sync"
	"time"

	"github.com/xssnick/tonutils-go/adnl"
	"github.com/xssnick/tonutils-go/adnl/dht"
	"github.com/xssnick/tonutils-go/adnl/keys"
)

const (
	tlVectorConstructor        = uint32(0x1cb5c415)
	tlAddressUDPConstructor    = uint32(0xe7a60d67)
	tlAddressUDPAltConstructor = uint32(0x53720178)
)

type resolvedAddress struct {
	IP      string `json:"ip"`
	Port    int32  `json:"port"`
	Version string `json:"version"`
}

type result struct {
	ADNLAddr       string            `json:"adnl_addr"`
	OwnerPublicKey string            `json:"owner_public_key,omitempty"`
	Addresses      []resolvedAddress `json:"addresses,omitempty"`
	Version        int32             `json:"version"`
	ReinitDate     int32             `json:"reinit_date"`
	Priority       int32             `json:"priority"`
	ExpireAt       int32             `json:"expire_at"`
	Error          string            `json:"error,omitempty"`
}

type batchRequest struct {
	ADNLAddrs []string `json:"adnl_addrs"`
}

type batchResponse struct {
	Results []result `json:"results"`
}

func main() {
	configURL := flag.String("config-url", "https://ton-blockchain.github.io/global.config.json", "TON global config URL")
	timeout := flag.Duration("timeout", 20*time.Second, "overall lookup timeout")
	perLookupTimeout := flag.Duration("per-lookup-timeout", 20*time.Second, "timeout for one DHT lookup in batch mode")
	batch := flag.Bool("batch", false, "read {\"adnl_addrs\":[...]} from stdin and resolve all keys")
	workers := flag.Int("workers", 8, "parallel DHT lookups in batch mode")
	debugRaw := flag.Bool("debug-raw", false, "print raw DHT address value on parse errors")
	flag.Parse()

	if *batch {
		if flag.NArg() != 0 {
			fail("usage: ton-dht-resolver --batch [--config-url URL] [--timeout 5m] [--per-lookup-timeout 20s] [--workers 8]")
		}
		runBatch(*configURL, *timeout, *perLookupTimeout, *workers, *debugRaw)
		return
	}

	if flag.NArg() != 1 {
		fail("usage: ton-dht-resolver [--config-url URL] [--timeout 20s] <adnl_addr_hex>")
	}

	client, closer := createClient(*configURL, *timeout)
	defer closer()

	out, err := resolveOne(context.Background(), client, flag.Arg(0), *debugRaw)
	if err != nil {
		fail("%v", err)
	}

	if err := json.NewEncoder(os.Stdout).Encode(out); err != nil {
		fail("failed to encode result: %v", err)
	}
}

func runBatch(configURL string, timeout time.Duration, perLookupTimeout time.Duration, workers int, debugRaw bool) {
	if workers < 1 {
		workers = 1
	}

	var req batchRequest
	if err := json.NewDecoder(os.Stdin).Decode(&req); err != nil {
		fail("failed to decode batch request: %v", err)
	}

	client, closer := createClient(configURL, timeout)
	defer closer()

	type job struct {
		index   int
		adnlHex string
	}

	jobs := make(chan job)
	results := make([]result, len(req.ADNLAddrs))
	var wg sync.WaitGroup

	for worker := 0; worker < workers; worker++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for item := range jobs {
				ctx, cancel := context.WithTimeout(context.Background(), perLookupTimeout)
				out, err := resolveOne(ctx, client, item.adnlHex, debugRaw)
				cancel()
				if err != nil {
					out = result{
						ADNLAddr: item.adnlHex,
						Error:    err.Error(),
					}
				}
				results[item.index] = out
			}
		}()
	}

	for index, adnlHex := range req.ADNLAddrs {
		jobs <- job{index: index, adnlHex: adnlHex}
	}
	close(jobs)
	wg.Wait()

	if err := json.NewEncoder(os.Stdout).Encode(batchResponse{Results: results}); err != nil {
		fail("failed to encode batch response: %v", err)
	}
}

func createClient(configURL string, timeout time.Duration) (*dht.Client, func()) {
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()

	_, privateKey, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		fail("failed to generate local ADNL key: %v", err)
	}

	gateway := adnl.NewGateway(privateKey)
	if err := gateway.StartClient(); err != nil {
		fail("failed to start ADNL gateway: %v", err)
	}

	client, err := dht.NewClientFromConfigUrl(ctx, gateway, configURL)
	if err != nil {
		_ = gateway.Close()
		fail("failed to create DHT client: %v", err)
	}

	return client, func() {
		client.Close()
		_ = gateway.Close()
	}
}

func resolveOne(ctx context.Context, client *dht.Client, adnlHex string, debugRaw bool) (result, error) {
	adnlKey, err := hex.DecodeString(adnlHex)
	if err != nil {
		return result{ADNLAddr: adnlHex}, fmt.Errorf("invalid adnl hex: %w", err)
	}
	if len(adnlKey) != 32 {
		return result{ADNLAddr: adnlHex}, fmt.Errorf("adnl key must be 32 bytes, got %d", len(adnlKey))
	}

	value, _, err := client.FindValue(ctx, &dht.Key{
		ID:    adnlKey,
		Name:  []byte("address"),
		Index: 0,
	})
	if err != nil {
		return result{ADNLAddr: adnlHex}, fmt.Errorf("failed to find address: %w", err)
	}

	addressList, err := parseAddressList(value.Data)
	if err != nil {
		if debugRaw {
			_, _ = fmt.Fprintf(os.Stderr, "raw_address_value_len=%d raw_address_value_hex=%s\n", len(value.Data), hex.EncodeToString(value.Data))
		}
		return result{ADNLAddr: adnlHex}, fmt.Errorf("failed to parse address list: %w", err)
	}

	keyID, ok := value.KeyDescription.ID.(keys.PublicKeyED25519)
	if !ok {
		return result{ADNLAddr: adnlHex}, fmt.Errorf("unsupported DHT owner key type %T", value.KeyDescription.ID)
	}

	out := result{
		ADNLAddr:       adnlHex,
		OwnerPublicKey: hex.EncodeToString(keyID.Key),
		Version:        addressList.Version,
		ReinitDate:     addressList.ReinitDate,
		Priority:       addressList.Priority,
		ExpireAt:       addressList.ExpireAt,
	}
	out.Addresses = addressList.Addresses

	return out, nil
}

type parsedAddressList struct {
	Addresses  []resolvedAddress
	Version    int32
	ReinitDate int32
	Priority   int32
	ExpireAt   int32
}

func parseAddressList(data []byte) (*parsedAddressList, error) {
	reader := tlReader{data: data}

	first, err := reader.readUint32()
	if err != nil {
		return nil, err
	}

	count, err := reader.readVectorCount(first)
	if err != nil {
		return nil, err
	}
	if count < 0 {
		return nil, fmt.Errorf("negative address count %d", count)
	}

	out := &parsedAddressList{}
	for i := int32(0); i < count; i++ {
		constructor, err := reader.readConstructor()
		if err != nil {
			return nil, err
		}

		switch constructor {
		case tlAddressUDPConstructor, tlAddressUDPAltConstructor:
			rawIP, err := reader.readUint32()
			if err != nil {
				return nil, err
			}
			port, err := reader.readInt32()
			if err != nil {
				return nil, err
			}
			ip := make(net.IP, net.IPv4len)
			binary.BigEndian.PutUint32(ip, rawIP)
			out.Addresses = append(out.Addresses, resolvedAddress{
				IP:      ip.String(),
				Port:    port,
				Version: "udp4",
			})
		default:
			return nil, fmt.Errorf("unsupported address constructor %08x", constructor)
		}
	}

	if out.Version, err = reader.readInt32(); err != nil {
		return nil, err
	}
	if out.ReinitDate, err = reader.readInt32(); err != nil {
		return nil, err
	}
	if out.Priority, err = reader.readInt32(); err != nil {
		return nil, err
	}
	if out.ExpireAt, err = reader.readInt32(); err != nil {
		return nil, err
	}

	return out, nil
}

type tlReader struct {
	data []byte
	off  int
}

func (r *tlReader) readUint32() (uint32, error) {
	if len(r.data)-r.off < 4 {
		return 0, io.ErrUnexpectedEOF
	}
	value := binary.LittleEndian.Uint32(r.data[r.off : r.off+4])
	r.off += 4
	return value, nil
}

func (r *tlReader) readConstructor() (uint32, error) {
	if len(r.data)-r.off < 4 {
		return 0, io.ErrUnexpectedEOF
	}
	value := binary.BigEndian.Uint32(r.data[r.off : r.off+4])
	r.off += 4
	return value, nil
}

func (r *tlReader) readInt32() (int32, error) {
	value, err := r.readUint32()
	return int32(value), err
}

func (r *tlReader) readVectorCount(first uint32) (int32, error) {
	switch {
	case first == tlVectorConstructor:
		return r.readInt32()
	case first < 1024:
		return int32(first), nil
	default:
		next, err := r.readUint32()
		if err != nil {
			return 0, err
		}
		if next == tlVectorConstructor {
			return r.readInt32()
		}
		return int32(next), nil
	}
}

func fail(format string, args ...any) {
	_, _ = fmt.Fprintf(os.Stderr, format+"\n", args...)
	os.Exit(1)
}
