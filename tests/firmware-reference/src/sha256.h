/* SHA-256 per FIPS 180-4; bounded streaming use, tested against host hashlib. */
#ifndef REF_SHA256_H
#define REF_SHA256_H
typedef struct { U32 h[8]; U64 total; U8 block[64]; U32 used; } SHA256_CTX;
static U32 rotr(U32 x,U32 n){return(x>>n)|(x<<(32-n));}
static U32 be32(const U8*p){return((U32)p[0]<<24)|((U32)p[1]<<16)|((U32)p[2]<<8)|p[3];}
static void sha_block(SHA256_CTX*c,const U8*b){
 static const U32 k[64]={
 0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,
 0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,
 0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
 0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,
 0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,
 0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
 0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,
 0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2};
 U32 w[64];for(U32 i=0;i<16;i++)w[i]=be32(b+4*i);
 for(U32 i=16;i<64;i++){U32 x=w[i-15],y=w[i-2];w[i]=w[i-16]+(rotr(x,7)^rotr(x,18)^(x>>3))+w[i-7]+(rotr(y,17)^rotr(y,19)^(y>>10));}
 U32 a=c->h[0],bb=c->h[1],cc=c->h[2],d=c->h[3],e=c->h[4],f=c->h[5],g=c->h[6],h=c->h[7];
 for(U32 i=0;i<64;i++){U32 t1=h+(rotr(e,6)^rotr(e,11)^rotr(e,25))+((e&f)^(~e&g))+k[i]+w[i];U32 t2=(rotr(a,2)^rotr(a,13)^rotr(a,22))+((a&bb)^(a&cc)^(bb&cc));h=g;g=f;f=e;e=d+t1;d=cc;cc=bb;bb=a;a=t1+t2;}
 c->h[0]+=a;c->h[1]+=bb;c->h[2]+=cc;c->h[3]+=d;c->h[4]+=e;c->h[5]+=f;c->h[6]+=g;c->h[7]+=h;
}
static void sha_init(SHA256_CTX*c){const U32 h[8]={0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19};for(U32 i=0;i<8;i++)c->h[i]=h[i];c->used=0;c->total=0;}
static void sha_update(SHA256_CTX*c,const U8*p,UINTN n){c->total+=n;while(n--){c->block[c->used++]=*p++;if(c->used==64){sha_block(c,c->block);c->used=0;}}}
static void sha_final(SHA256_CTX*c,U8 out[32]){U64 bits=c->total*8;c->block[c->used++]=0x80;if(c->used>56){while(c->used<64)c->block[c->used++]=0;sha_block(c,c->block);c->used=0;}while(c->used<56)c->block[c->used++]=0;for(U32 i=0;i<8;i++)c->block[63-i]=(U8)(bits>>(8*i));sha_block(c,c->block);for(U32 i=0;i<8;i++)for(U32 j=0;j<4;j++)out[4*i+j]=(U8)(c->h[i]>>(24-8*j));}
#endif
