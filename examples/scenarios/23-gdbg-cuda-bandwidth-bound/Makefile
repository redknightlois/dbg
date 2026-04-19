NVCC = nvcc
NVCCFLAGS = -O2 -lineinfo

broken: broken.cu
	$(NVCC) $(NVCCFLAGS) -o $@ $<

clean:
	rm -f broken
